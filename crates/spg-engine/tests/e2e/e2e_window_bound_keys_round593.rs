//! v7.39 (round 593) — the window pipeline resolved its keys and its
//! argument by NAME, once per row.
//!
//! Round 592 left this needing a profile it could not get: its symbolication
//! put SHA-512 at 23% of a `lag()` query, which nothing in the read path can
//! call. That round blamed the load base. It was not the base — `atos
//! -offset` and `-l 0x100000000` agree to the byte. The mistake was mapping
//! EVERY frame against the probe binary: samply reports each frame's library
//! through `funcTable.resource -> resourceTable.lib`, and the frames blamed
//! on SHA-512 belong to `libsystem_malloc.dylib`. Attributed properly, the
//! same profile reads:
//!
//!     SELECT count(*) FROM j                      malloc  0.2%
//!     … (SELECT id FROM j ORDER BY id) q          malloc 47.1%
//!     … (SELECT id, lag(id) OVER (ORDER BY id))   malloc 28.4%, engine 60.2%
//!
//! and inside the engine's own 60.2%: `resolve_column` 5.80%, `eval_expr`
//! 2.55%, `rehydrate_cell` 2.45% — a tenth of the query spent looking up
//! which column a key names, for every row.
//!
//! A key that is a plain column sits at the same position in every row, so
//! it is resolved once. Round 582 established this for the top-level ORDER
//! BY; the same helper now serves the window's PARTITION BY, its ORDER BY and
//! its function argument. Engine-side, 200k rows:
//!
//!     OVER ()                            24.1 -> 19.5 ms
//!     OVER (ORDER BY id)                 33.3 -> 26.4
//!     OVER (PARTITION BY g)              61.6 -> 55.1
//!     OVER (PARTITION BY g ORDER BY id) 108.6 -> 91.4
//!
//! Over pgwire on 500k rows, `lag(id) OVER (ORDER BY id)` reads 87.5 ms
//! against PG18's 6.55, where round 592 left it at 89.66 — the saving is
//! smaller at that size because the sort's share grows with n log n.
//! 18.7x at the start of round 592, 13.4x now.
//!
//! What the pins are for. Reading a cell directly is only the same answer
//! while the position really is fixed and the resolver would have done
//! nothing else: a qualifier naming a different relation, a name that is
//! ambiguous in the schema, an expression rather than a column, and a
//! composite column — which is stored as JSON and rebuilt on the way out, so
//! its raw cell is not its value — all stay on the resolver. Every shape here
//! was checked against live PG18 and matched.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bd (id INT, p INT, v INT, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO bd SELECT gg, gg % 3, CASE WHEN gg % 4 = 0 THEN NULL ELSE gg * 2 END, \
         CASE WHEN gg % 5 = 0 THEN NULL ELSE 'k' || (gg % 7) END FROM generate_series(1, 60) gg",
    )
    .unwrap();
    e
}

/// The bound path and the resolver have to agree, whichever way the columns
/// are written: bare, qualified by the table's own name, or by an alias.
#[test]
fn round593_qualified_and_aliased_keys_agree() {
    let mut e = seed();
    let want = vec![
        "1|NULL", "2|NULL", "3|NULL", "4|2", "5|4", "6|6", "7|NULL", "8|10",
    ];
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(v) OVER (PARTITION BY p ORDER BY id) FROM bd WHERE id <= 8 ORDER BY id"
        ),
        want,
        "bare"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(bd.v) OVER (PARTITION BY bd.p ORDER BY bd.id) FROM bd \
             WHERE id <= 8 ORDER BY id"
        ),
        want,
        "qualified by the table name"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(t.v) OVER (PARTITION BY t.p ORDER BY t.id) FROM bd t \
             WHERE id <= 8 ORDER BY id"
        ),
        want,
        "qualified by an alias"
    );
    // Renamed by a derived table: the positions are the derived schema's.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a, lag(b) OVER (PARTITION BY c ORDER BY a) FROM \
             (SELECT id AS a, v AS b, p AS c FROM bd) q WHERE a <= 8 ORDER BY a"
        ),
        want
    );
    // And across a join, where every reference has to carry its qualifier.
    assert_eq!(
        vals(
            &mut e,
            "SELECT x.id, lag(y.v) OVER (PARTITION BY x.p ORDER BY x.id) FROM bd x \
             JOIN bd y ON x.id = y.id WHERE x.id <= 6 ORDER BY x.id"
        ),
        vec!["1|NULL", "2|NULL", "3|NULL", "4|2", "5|4", "6|6"]
    );
}

/// Anything that is not a plain column keeps the resolver — an expression in
/// the key, or in the argument.
#[test]
fn round593_expressions_keep_the_resolver() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p % 2 ORDER BY id * -1) FROM bd \
             WHERE id <= 8 ORDER BY id"
        ),
        vec![
            "1|16", "2|32", "3|28", "4|14", "5|22", "6|12", "7|14", "8|NULL",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(v + 1) OVER (ORDER BY id), sum(v * 2) OVER (PARTITION BY p) \
             FROM bd WHERE id <= 6 ORDER BY id"
        ),
        vec![
            "1|NULL|4",
            "2|3|28",
            "3|5|36",
            "4|7|4",
            "5|NULL|28",
            "6|11|36",
        ]
    );
}

/// NULLs and text through the bound path, and the frame functions that read
/// the argument at a position rather than in order.
#[test]
fn round593_nulls_text_and_positional_frames() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(s) OVER (PARTITION BY s ORDER BY id), count(s) OVER (PARTITION BY s) \
             FROM bd WHERE id <= 10 ORDER BY id"
        ),
        vec![
            "1|NULL|2",
            "2|NULL|2",
            "3|NULL|1",
            "4|NULL|1",
            "5|NULL|0",
            "6|NULL|1",
            "7|NULL|1",
            "8|k1|2",
            "9|k2|2",
            "10|NULL|0",
        ],
        "a NULL text key partitions with the other NULLs and counts none of them"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, first_value(v) OVER (PARTITION BY p ORDER BY v DESC NULLS LAST) \
             FROM bd WHERE id <= 9 ORDER BY id"
        ),
        vec![
            "1|14", "2|10", "3|18", "4|14", "5|10", "6|18", "7|14", "8|10", "9|18",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, nth_value(v, 2) OVER (PARTITION BY p ORDER BY id), \
             last_value(v) OVER (PARTITION BY p ORDER BY id) FROM bd WHERE id <= 9 ORDER BY id"
        ),
        vec![
            "1|NULL|2",
            "2|NULL|4",
            "3|NULL|6",
            "4|NULL|NULL",
            "5|10|10",
            "6|12|12",
            "7|NULL|14",
            "8|10|NULL",
            "9|12|18",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, count(*) OVER (PARTITION BY p), count(v) OVER (PARTITION BY p) \
             FROM bd WHERE id <= 6 ORDER BY id"
        ),
        vec!["1|2|1", "2|2|2", "3|2|2", "4|2|1", "5|2|2", "6|2|2"],
        "count(*) has no argument to bind"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (), max(s) OVER () FROM bd WHERE id <= 4 ORDER BY id"
        ),
        vec!["1|12|k4", "2|12|k4", "3|12|k4", "4|12|k4"],
        "no keys at all to bind"
    );
}

/// Over the whole input, by sums rather than by listing — a key read from
/// the wrong position would move these.
#[test]
fn round593_whole_input_checksums() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT sum(id), sum(l), count(l), sum(t) FROM \
             (SELECT id, lag(v) OVER (PARTITION BY p ORDER BY id) l, \
              sum(v) OVER (PARTITION BY p) t FROM bd) q"
        ),
        vec!["1830|2466|43|54000"]
    );
    // At a size where the sort actually runs, against the same answer
    // computed by GROUP BY.
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e2.execute("INSERT INTO big SELECT gg, gg % 11 FROM generate_series(1, 3000) gg")
        .unwrap();
    assert_eq!(
        vals(
            &mut e2,
            "SELECT DISTINCT g, sum(id) OVER (PARTITION BY g) FROM big ORDER BY g"
        ),
        vals(&mut e2, "SELECT g, sum(id) FROM big GROUP BY g ORDER BY g")
    );
    let run = vals(
        &mut e2,
        "SELECT lag(id) OVER (ORDER BY id) FROM big ORDER BY id",
    );
    assert_eq!(run.len(), 3000);
    assert_eq!(run[0], "NULL");
    assert_eq!(run[2999], "2999");
}
