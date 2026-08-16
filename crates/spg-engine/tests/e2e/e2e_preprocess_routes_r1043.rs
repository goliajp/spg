//! r1043 — every route into the engine gets the same pre-pass.
//!
//! There were four places that parsed a SQL string and then applied a
//! list of pre-passes by hand, and the lists had drifted. `prepare` had
//! clock rewrites, `GROUP BY ALL` expansion, ORDER BY position
//! resolution and the JOIN reorder; the two streaming read paths had the
//! same list minus the `GROUP BY ALL` expansion; and r1042 added
//! constant folding to exactly one of them.
//!
//! `execute_readonly_select_streaming` is the route EVERY autocommit
//! SELECT takes over the wire, and it was one of the ones without the
//! fold. So r1042's whole change reached the embedded API and `EXPLAIN`
//! and never reached a client. Measured on the same connection, same
//! build, 400,000 rows:
//!
//! ```text
//! SELECT count(*) … WHERE b = decode(lpad(to_hex(7),16,'0'),'hex')   198 ms
//! EXPLAIN ANALYZE of the identical statement       Execution Time: 0.013 ms
//! ```
//!
//! EXPLAIN went through `prepare`. The query did not. After: 0.066 ms.
//!
//! What is pinned is that the ROUTES agree — a pass added to
//! `Engine::preprocess` reaches all of them by construction, and this
//! test is what notices if a fifth route grows its own copy.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pp (id INT PRIMARY KEY, b BYTEA, k INT)")
        .unwrap();
    e.execute(
        "INSERT INTO pp VALUES (7, '\\x0000000000000007', 1), \
                              (8, '\\x0000000000000008', 2)",
    )
    .unwrap();
    e.execute("CREATE INDEX pp_b ON pp (b)").unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn ro_rows(e: &Engine, sql: &str) -> Vec<String> {
    match e
        .execute_readonly(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The statement the streaming read path prepares is folded, which is
/// the one that was not.
#[test]
fn r1043_the_streaming_read_route_folds_too() {
    let e = engine();
    let s = e
        .prepare_select_streaming(
            "SELECT id FROM pp WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')",
        )
        .expect("prepares");
    let text = format!("{s}");
    assert!(
        !text.contains("decode"),
        "the streaming route did not fold: {text}"
    );
    assert!(text.contains(r"'\x0000000000000007'"), "{text}");
}

/// `prepare` and the streaming route produce the same statement. Two
/// routes with two lists is the defect; equality is the property.
#[test]
fn r1043_the_routes_agree() {
    let e = engine();
    for sql in [
        "SELECT id FROM pp WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')",
        "SELECT id FROM pp WHERE id = 3 + 4",
        "SELECT id FROM pp WHERE id = 7::int ORDER BY 1",
    ] {
        let via_prepare = format!("{}", e.prepare(sql).expect("prepare"));
        let via_stream = format!(
            "SELECT {}",
            e.prepare_select_streaming(sql)
                .expect("prepare_select_streaming")
        );
        // `prepare` returns a Statement and the streaming route a
        // SelectStatement; compare the part they both render.
        let a = via_prepare.trim_start_matches("SELECT ").to_string();
        let b = via_stream.trim_start_matches("SELECT ").to_string();
        assert_eq!(a, b, "routes disagree on:\n{sql}");
    }
}

/// An immutable built-in folds; the answer is the answer either way.
#[test]
fn r1043_an_immutable_call_folds_and_answers_the_same() {
    let mut e = engine();
    let folded = rows(
        &mut e,
        "SELECT id FROM pp WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')",
    );
    let literal = rows(&mut e, "SELECT id FROM pp WHERE b = '\\x0000000000000007'");
    assert_eq!(folded, literal);
    assert_eq!(folded, vec!["7".to_string()]);
    // …and through the read-only route, which is the one the wire takes.
    assert_eq!(
        ro_rows(
            &e,
            "SELECT id FROM pp WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')"
        ),
        vec!["7".to_string()]
    );
}

/// `||` is context-free and has to be on the list.
///
/// It was missing, and the omission was invisible: the context test is
/// ANDed with the shape test, so the narrower of the two silently wins.
/// `decode(…, chr(104)||chr(101)||chr(120))` measured 373 ms against
/// 0.143 for the same value spelled `'hex'`.
#[test]
fn r1043_concatenation_of_constants_folds() {
    let e = engine();
    let s = e
        .prepare_select_streaming(
            "SELECT id FROM pp WHERE b = decode(lpad(to_hex(7), 16, chr(48)), \
             chr(104) || chr(101) || chr(120))",
        )
        .expect("prepares");
    let text = format!("{s}");
    assert!(
        !text.contains("chr("),
        "the concatenation did not fold: {text}"
    );
}

/// A function NOT on the immutable list stays unfolded. The list is a
/// positive one precisely so an unclassified function fails this way
/// rather than by being folded wrongly.
#[test]
fn r1043_an_unlisted_function_is_not_folded() {
    let e = engine();
    let s = e
        .prepare_select_streaming("SELECT id FROM pp WHERE k = length(upper('ab'))")
        .expect("prepares");
    let text = format!("{s}");
    assert!(
        text.contains("upper") || text.contains("length"),
        "an unclassified function was folded: {text}"
    );
}
