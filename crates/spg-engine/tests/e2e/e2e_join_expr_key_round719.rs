//! Round 719 — a single integer-only COMPUTED join key takes the i64
//! hash lane (S07 family ①). `ON a.id = b.id + 1` used to pay three
//! taxes the plain-column key did not: a canonical-string hash table,
//! the conjunct re-verified in `residual` against a materialised
//! combined row per matching pair, and an interpreted eval per build
//! row — 5.29× against PG on the 500k self-join, 0.70× after.
//!
//! These pins hold the lane's ANSWERS: every shape ran through the
//! round-719 differential against PG18 (12/12 byte-same, including
//! LEFT / RIGHT / FULL null-extension and the mixed-conjunct form
//! that must NOT take the lane).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE j719 (id INT, g INT)").unwrap();
    e.execute("INSERT INTO j719 SELECT gg, gg % 3 FROM generate_series(1, 100) gg")
        .unwrap();
}

#[test]
fn round719_int_expr_key_joins_answer_as_pg() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        // id = id + 1: 99 pairs (2..=100 each match one predecessor).
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id + 1",
            "99",
        ),
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id - 3",
            "97",
        ),
        // id = id * 2: even ids 2..=100 -> 50 pairs.
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id * 2",
            "50",
        ),
        // Anti-join: only a.id = 1 has no b with b.id + 1 = 1.
        (
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON a.id = b.id + 1 \
             WHERE b.id IS NULL",
            "1",
        ),
        // RIGHT: only b.id = 100 has no a with a.id = 101.
        (
            "SELECT count(*) FROM j719 a RIGHT JOIN j719 b ON a.id = b.id + 1 \
             WHERE a.id IS NULL",
            "1",
        ),
        // A SECOND conjunct keeps the residual: id = id + 1 pairs whose
        // g also matches — id ≡ id+1 (mod 3) never holds, so zero.
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id + 1 AND a.g = b.g",
            "0",
        ),
        // The winning row's VALUES, not just counts.
        (
            "SELECT sum(a.id) FROM j719 a JOIN j719 b ON a.id = b.id + 99",
            "100",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// NULLs on either side of the computed key join nothing (SQL `=`).
#[test]
fn round719_null_keys_never_match() {
    let mut e = Engine::new();
    seed(&mut e);
    e.execute("INSERT INTO j719 VALUES (NULL, 0), (NULL, 1)")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id + 1"
        ),
        "99",
        "NULL build keys and NULL probe keys both stay out"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON a.id = b.id + 1 \
             WHERE b.id IS NULL"
        ),
        "3",
        "the two NULL-id left rows null-extend, plus a.id = 1"
    );
}

/// v7.39 (round 720) — the MIRROR shape: `<peer column> = <integer-only
/// expression over the joined left side>` (`ON b.id = a.id + 1` — what
/// the EXISTS pull-up emits). Build hashes the peer column, the probe
/// evaluates the left expression per tuple with zero materialisation.
/// The mixed-conjunct form below is the round-720 regression itself: a
/// plain eq_pair beside a probe-side computed key briefly let int_keyed
/// hash on the pair ALONE — 5000-row buckets re-verified per probe, a
/// resurrected round-590 quadratic (the differential hung). Both key
/// halves must land in the composite key.
#[test]
fn round720_mirror_int_expr_key_joins_answer_as_pg() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON b.id = a.id + 1",
            "99",
        ),
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON b.id = a.id * 2",
            "50",
        ),
        // Mixed: id offset by one never keeps g = id % 3 equal.
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON b.id = a.id + 1 AND b.g = a.g",
            "0",
        ),
        // Mixed where SOME survive: b.id = a.id + 3 preserves g (mod 3).
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON b.id = a.id + 3 AND b.g = a.g",
            "97",
        ),
        (
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON b.id = a.id + 99              WHERE b.id IS NULL",
            "99",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 721) — the ANTI-join pull-up admits a COMPUTED outer
/// correlation half: `NOT EXISTS (SELECT 1 FROM t b WHERE b.id = a.id +
/// K)` becomes LEFT JOIN ON b.id = a.id + K (the round-720 mirror hash
/// shape) + IS NULL, instead of bailing to the per-row correlated
/// executor. Admission bar: negated only, both sides integer-family
/// (checked against the catalog at extraction). Expectations measured
/// against PG18 in the round-721 differential.
#[test]
fn round721_not_exists_computed_key_pulls_up() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        // b.id = a.id + 99 exists only for a.id = 1 -> 99 rows survive.
        (
            "SELECT count(*) FROM j719 a WHERE NOT EXISTS              (SELECT 1 FROM j719 b WHERE b.id = a.id + 99)",
            "99",
        ),
        (
            "SELECT count(*) FROM j719 a WHERE NOT EXISTS              (SELECT 1 FROM j719 b WHERE b.id = a.id * 2)",
            "50",
        ),
        // An all-inner residual rides along in the ON.
        (
            "SELECT count(*) FROM j719 a WHERE NOT EXISTS              (SELECT 1 FROM j719 b WHERE b.id = a.id + 1 AND b.g = 0)",
            "67",
        ),
        // sum pins the surviving VALUES.
        (
            "SELECT sum(a.id) FROM j719 a WHERE NOT EXISTS              (SELECT 1 FROM j719 b WHERE b.id = a.id + 50)",
            "3775",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 725) — positive EXISTS pulls up as a true SEMI join
/// (`JoinKind::Semi`: each outer row keeps at most one pairing), which
/// retires the round-721 uniqueness gate — an INNER join multiplied
/// outer rows on duplicate inner matches, a semi join cannot. The
/// duplicate-heavy shapes here are the exact multiplication trap:
/// g = id % 3 has ~33 duplicates per value, and no column carries a
/// declared UNIQUE. All PG18-measured (round-725 differential, 8/8).
#[test]
fn round725_exists_semi_join_never_multiplies() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        // Duplicate-bucket probe: every a.id <= 97 has b.id = a.id + 3.
        (
            "SELECT count(*) FROM j719 a WHERE EXISTS              (SELECT 1 FROM j719 b WHERE b.id = a.id + 3)",
            "97",
        ),
        // Massive-duplicate inner key (b.g: 33 rows per value): the
        // count must be the OUTER row count, not the pairing count.
        (
            "SELECT count(*) FROM j719 a WHERE EXISTS              (SELECT 1 FROM j719 b WHERE b.g = a.g) AND a.id <= 10",
            "10",
        ),
        // Computed key + all-inner residual, positive form (the lifted
        // round-721 restriction).
        (
            "SELECT count(*) FROM j719 a WHERE EXISTS              (SELECT 1 FROM j719 b WHERE b.id = a.id + 1 AND b.g = 0)",
            "33",
        ),
        // Values, not just counts.
        (
            "SELECT sum(a.id) FROM j719 a WHERE EXISTS              (SELECT 1 FROM j719 b WHERE b.id = a.id * 2)",
            "1275",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 732) — TWO integer keys pack into one exact i128 (the
/// EXISTS pull-up's mixed shape: a plain pair beside a computed key),
/// and on every lane where a probe-expr's value IS in the key, its
/// conjunct leaves residual. Round-732 differential 6/6 byte-same;
/// these pin the mixed-key answers, LEFT null-extension included.
#[test]
fn round732_two_int_keys_answer_as_pg() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        // g preserved by +3 (mod 3): 97 pairs.
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON b.g = a.g AND b.id = a.id + 3",
            "97",
        ),
        // g broken by +1: zero.
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON b.g = a.g AND b.id = a.id + 1",
            "0",
        ),
        (
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON b.g = a.g AND b.id = a.id + 3              WHERE b.id IS NULL",
            "3",
        ),
        (
            "SELECT count(*) FROM j719 a WHERE EXISTS              (SELECT 1 FROM j719 b WHERE b.g = a.g AND b.id = a.id + 3)",
            "97",
        ),
        (
            "SELECT sum(b.id) FROM j719 a JOIN j719 b ON b.g = a.g AND b.id = a.id + 99",
            "100",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 744) — the count-star anti-join fast path accepts a
/// COMPUTED inner key (`ON a.id = b.id + 1 WHERE b.id IS NULL`), with
/// the IS NULL column constrained to one the key expression READS —
/// any other inner column could be NULL on a matched row and the count
/// would be wrong (that shape keeps the general path, pinned).
#[test]
fn round744_anti_join_fast_takes_computed_keys() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        (
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON a.id = b.id + 1              WHERE b.id IS NULL",
            "1",
        ),
        (
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON a.id = b.id * 2              WHERE b.id IS NULL",
            "50",
        ),
        // g is NOT read by the key: general path, and NULL-g matched
        // rows must not be counted (none here — g is never NULL).
        (
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON a.id = b.id + 1              WHERE b.g IS NULL",
            "1",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 745) — peer-only residual predicates filter the hash
/// BUILD up front (ON-clause semantics: a rejected build row pads
/// exactly like an absent one). LEFT pads and mixed ON predicates are
/// the risk surface — all PG18-measured (round-745 differential 6/6).
#[test]
fn round745_build_side_predicates_answer_as_pg() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id              WHERE a.g = 0 AND b.g = 0",
            "33",
        ),
        // LEFT: rows failing the ON's peer predicate still pad.
        (
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON a.id = b.id AND b.g = 0              WHERE a.id <= 10",
            "10",
        ),
        (
            "SELECT count(b.id) FROM j719 a LEFT JOIN j719 b ON a.id = b.id AND b.g = 0              WHERE a.id <= 10",
            "3",
        ),
        (
            "SELECT sum(b.id) FROM j719 a JOIN j719 b ON a.id = b.id AND b.g = 1              WHERE a.id <= 10",
            "22",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 746) — the integer-lane hash BUILD shards across the
/// parallel runner (local tables merged in shard order, preserving
/// ascending row order inside every bucket). This engine has no runner,
/// so these exercise the serial walk — the answers the sharded build
/// must reproduce are pinned by the round-746 differential (7/7,
/// including bucket-order-sensitive projections).
#[test]
fn round746_build_answers_hold() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id              WHERE a.g = 0 AND b.g = 0",
            "33",
        ),
        // Duplicate-key bucket order feeds output order.
        (
            "SELECT b.g FROM j719 a JOIN j719 b ON a.g = b.g              WHERE a.id = 1 AND b.id <= 6 ORDER BY b.id",
            "1 / 1",
        ),
    ] {
        assert_eq!(one_join(&mut e, sql, want), true, "{sql}");
    }
}

fn one_join(e: &mut Engine, sql: &str, want: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            let got = rows
                .iter()
                .map(|r| {
                    r.values
                        .iter()
                        .map(spg_engine::eval::value_to_text)
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect::<Vec<_>>()
                .join(" / ");
            got == want || {
                eprintln!("got {got:?} want {want:?}");
                false
            }
        }
        other => panic!("{other:?}"),
    }
}
