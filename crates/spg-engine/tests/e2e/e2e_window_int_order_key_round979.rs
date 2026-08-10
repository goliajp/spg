//! r979 — `row_number() OVER (ORDER BY <int col>)` sorts on the i64, not
//! on a heap vector per row.
//!
//! Round 977 showed the ordered window's cost is key-shaped rather than
//! row-shaped: the sort's share was 132.0 ms on a three-integer table at
//! 400k rows and 132.5 with a 200-byte column added, while a per-row COPY
//! does scale with width (round 976 measured +36 ns/row/200 bytes).
//! Round 978 priced the cheaper key by ablation at 79.8%; the real thing
//! measures 74.1% with a clean control leg, the difference being the
//! correctness the ablation skipped and this file pins.
//!
//! WHAT THESE PINS WITNESS. Not that the fast path RAN — for `row_number`
//! that is unobservable by design, and the A/B is what shows it. Two
//! other things, both of which the ablation got wrong:
//!
//!   * the four NULL/direction combinations, ties, and the minimum of
//!     each integer width, because the fast path re-implements ordering
//!     the general comparator already had (and a negate-based direction
//!     would overflow on those minima);
//!   * the GATE. The fast path leaves the entries' order-key vectors
//!     empty and `rank` / `dense_rank` compare exactly those, so if
//!     either became eligible every row would come back rank 1. The rank
//!     pins are a witness that the gate holds, not merely a correctness
//!     check.
//!
//! WHERE THESE NUMBERS COME FROM. The engine, before and after the
//! change, verified byte-identical on all 19 shapes — NOT from working
//! them out by hand, which produced a wrong expectation the first time
//! this file was written.
//!
//! One thing they are NOT: PG18.4's answer for the two NULL rows. SPG
//! orders equal keys by the order the scan produced (id 2 before id 5);
//! PG orders those two the other way. That is a tie between rows whose
//! ORDER BY keys are equal, which SQL does not define and PG does not
//! promise across plans, and it predates this change — measured on the
//! before-binary, round 979. Everything the two servers do define, they
//! agree on.

use spg_engine::Engine;

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

/// The window value per row, in id order, as `id:v` (or `id:v:v`).
fn pairs(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Int(n) => n.to_string(),
                        spg_storage::Value::BigInt(n) => n.to_string(),
                        spg_storage::Value::Null => "NULL".to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Ties on 5 and on 1, two NULLs, and the minimum of each integer width.
fn seeded() -> Engine {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE tie (id INT PRIMARY KEY, k INT, b BIGINT, sm SMALLINT, s TEXT)",
    );
    run(
        &mut e,
        "INSERT INTO tie VALUES (1,5,5,5,'a'),(2,NULL,NULL,NULL,'b'),(3,5,5,5,'c'),\
         (4,1,1,1,'d'),(5,NULL,NULL,NULL,'e'),(6,9,9,9,'f'),(7,1,1,1,'g'),\
         (8,-2147483648,-9223372036854775808,-32768,'h')",
    );
    e
}

#[test]
fn ascending_puts_nulls_last_and_the_minimum_first() {
    let mut e = seeded();
    // id 8 holds INT_MIN, so it is row number 1; the two NULLs take the
    // last two numbers.
    let want = "1:4 2:7 3:5 4:2 5:8 6:6 7:3 8:1";
    assert_eq!(
        pairs(&mut e, "SELECT id, row_number() OVER (ORDER BY k) FROM tie"),
        want
    );
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY k NULLS LAST) FROM tie"
        ),
        want,
        "NULLS LAST is the ascending default, so spelling it changes nothing"
    );
}

#[test]
fn nulls_first_moves_them_and_shifts_the_rest() {
    let mut e = seeded();
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY k NULLS FIRST) FROM tie"
        ),
        "1:6 2:1 3:7 4:4 5:2 6:8 7:5 8:3"
    );
}

#[test]
fn descending_defaults_to_nulls_first_and_can_be_overridden() {
    let mut e = seeded();
    let want_first = "1:4 2:1 3:5 4:6 5:2 6:3 7:7 8:8";
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY k DESC) FROM tie"
        ),
        want_first
    );
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY k DESC NULLS FIRST) FROM tie"
        ),
        want_first,
        "NULLS FIRST is the descending default"
    );
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY k DESC NULLS LAST) FROM tie"
        ),
        "1:2 2:7 3:3 4:4 5:8 6:1 7:5 8:6",
        "and the query can say otherwise"
    );
}

#[test]
fn ties_keep_the_order_the_scan_produced_in_both_directions() {
    let mut e = seeded();
    // ids 4 and 7 tie at k=1; ids 1 and 3 tie at k=5. In BOTH directions
    // the earlier row takes the smaller number — an implementation that
    // sorted ascending and then reversed would swap each pair.
    let asc = pairs(&mut e, "SELECT id, row_number() OVER (ORDER BY k) FROM tie");
    assert!(
        asc.contains("4:2") && asc.contains("7:3"),
        "asc k=1 tie: {asc}"
    );
    assert!(
        asc.contains("1:4") && asc.contains("3:5"),
        "asc k=5 tie: {asc}"
    );

    let desc = pairs(
        &mut e,
        "SELECT id, row_number() OVER (ORDER BY k DESC NULLS LAST) FROM tie",
    );
    assert!(
        desc.contains("1:2") && desc.contains("3:3"),
        "desc k=5 tie: {desc}"
    );
    assert!(
        desc.contains("4:4") && desc.contains("7:5"),
        "desc k=1 tie: {desc}"
    );
}

#[test]
fn the_minimum_of_each_integer_width_sorts_as_the_smallest() {
    let mut e = seeded();
    // INT_MIN, BIGINT_MIN and SMALLINT_MIN all live on id 8.
    for col in ["k", "b", "sm"] {
        let got = pairs(
            &mut e,
            &format!("SELECT id, row_number() OVER (ORDER BY {col}) FROM tie"),
        );
        assert!(
            got.contains("8:1"),
            "{col}: the minimum must be first — {got}"
        );
        let got_desc = pairs(
            &mut e,
            &format!("SELECT id, row_number() OVER (ORDER BY {col} DESC NULLS LAST) FROM tie"),
        );
        assert!(
            got_desc.contains("8:6"),
            "{col} desc: the minimum must be last among non-NULLs — {got_desc}"
        );
    }
}

#[test]
fn rank_and_dense_rank_are_not_eligible_and_still_see_their_keys() {
    let mut e = seeded();
    // These compare the order-key vectors the fast path leaves empty, so
    // every row would be rank 1 if the gate ever let them through.
    assert_eq!(
        pairs(&mut e, "SELECT id, rank() OVER (ORDER BY k) FROM tie"),
        "1:4 2:7 3:4 4:2 5:7 6:6 7:2 8:1"
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, dense_rank() OVER (ORDER BY k) FROM tie"),
        "1:3 2:5 3:3 4:2 5:5 6:4 7:2 8:1"
    );
}

#[test]
fn row_number_beside_rank_in_one_query_answers_both() {
    let mut e = seeded();
    // Each window node decides for itself, so one statement can take the
    // fast path for one of them and not the other.
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY k), rank() OVER (ORDER BY k) FROM tie"
        ),
        "1:4:4 2:7:7 3:5:4 4:2:2 5:8:7 6:6:6 7:3:2 8:1:1"
    );
}

#[test]
fn the_shapes_the_gate_declines_still_answer() {
    let mut e = seeded();
    assert_eq!(
        pairs(&mut e, "SELECT id, row_number() OVER (ORDER BY s) FROM tie"),
        "1:1 2:2 3:3 4:4 5:5 6:6 7:7 8:8",
        "a text key is outside the gate"
    );
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (PARTITION BY k ORDER BY id) FROM tie"
        ),
        "1:1 2:1 3:2 4:1 5:2 6:1 7:2 8:1",
        "a partition is outside the gate"
    );
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY k, id) FROM tie"
        ),
        "1:4 2:7 3:5 4:2 5:8 6:6 7:3 8:1",
        "more than one key is outside the gate"
    );
    assert_eq!(
        pairs(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY k ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM tie"
        ),
        "1:4 2:7 3:5 4:2 5:8 6:6 7:3 8:1",
        "an explicit frame is outside the gate"
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, lag(id) OVER (ORDER BY k) FROM tie"),
        "1:7 2:6 3:1 4:8 5:2 6:3 7:4 8:NULL",
        "another ordered function is outside the gate"
    );
}
