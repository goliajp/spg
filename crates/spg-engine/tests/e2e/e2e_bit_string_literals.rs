//! B'1010' / X'1F' bit-string literals — lowered onto ::bit.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn binary_form() {
    let mut e = Engine::new();
    let v = one(&mut e, "SELECT B'1010'");
    let spg_storage::Value::BitString { nbits, bytes } = v else {
        panic!("expected BitString, got {v:?}");
    };
    assert_eq!(nbits, 4);
    assert_eq!(bytes[0] >> 4, 0b1010);
    // Bad digit errors.
    assert!(e.execute("SELECT B'102'").is_err());
}

#[test]
fn hex_form_expands_to_bits() {
    let mut e = Engine::new();
    // X'1F' = 0001 1111 — 8 bits.
    let v = one(&mut e, "SELECT X'1F'");
    let spg_storage::Value::BitString { nbits, bytes } = v else {
        panic!("expected BitString, got {v:?}");
    };
    assert_eq!(nbits, 8);
    assert_eq!(bytes[0], 0x1F);
    // Case-insensitive prefix + digits.
    let v = one(&mut e, "SELECT x'a'");
    let spg_storage::Value::BitString { nbits, .. } = v else {
        panic!("expected BitString, got {v:?}");
    };
    assert_eq!(nbits, 4);
    assert!(e.execute("SELECT X'G1'").is_err());
}

#[test]
fn bit_concat_yields_bitstring() {
    // PG `bit || bit` = a bit-varying of length nbits(a)+nbits(b); the
    // second operand's bits shift to start at offset nbits(a) (cross-byte
    // aware). SPG previously fell through to text concat → Text. Values
    // are live-PG18.4-verified.
    let mut e = Engine::new();
    let txt = |e: &mut Engine, sql: &str| -> String {
        match one(e, sql) {
            spg_storage::Value::Text(s) => s.to_string(),
            o => panic!("{sql}: expected Text, got {o:?}"),
        }
    };
    // The result must be a BitString (not Text) and hold the right bits.
    assert!(matches!(
        one(&mut e, "SELECT B'10' || B'11'"),
        spg_storage::Value::BitString { .. }
    ));
    assert_eq!(txt(&mut e, "SELECT (B'10' || B'11')::text"), "1011");
    assert_eq!(txt(&mut e, "SELECT (B'1010' || B'0101')::text"), "10100101");
    // Cross-byte alignment (first operand not byte-aligned).
    assert_eq!(txt(&mut e, "SELECT (B'101' || B'1')::text"), "1011");
    assert_eq!(
        txt(&mut e, "SELECT (B'11111111' || B'0000')::text"),
        "111111110000"
    );
    assert_eq!(txt(&mut e, "SELECT (B'' || B'11')::text"), "11");
    // Equality with the literal confirms the packed bits.
    assert_eq!(
        one(&mut e, "SELECT (B'101' || B'1') = B'1011'"),
        spg_storage::Value::Bool(true)
    );
}

#[test]
fn pg_typeof_bit_reports_bit_varying_not_unknown() {
    // SPG carries bit / bit varying in one BitString variant, so
    // pg_typeof reports "bit varying" (its data_type) — matching PG for
    // varbit values and the concat result. (A `bit` literal reads as
    // "bit varying" here vs PG's "bit"; that needs a fixed-vs-varying tag
    // SPG doesn't keep.) The point of this test is that it is no longer
    // "unknown". Live-PG18.4: pg_typeof('101'::varbit) = "bit varying".
    let mut e = Engine::new();
    let t = |e: &mut Engine, sql: &str| -> String {
        match one(e, sql) {
            spg_storage::Value::Text(s) => s.to_string(),
            o => panic!("{o:?}"),
        }
    };
    assert_eq!(
        t(&mut e, "SELECT pg_typeof('101'::varbit)::text"),
        "bit varying"
    );
    assert_eq!(
        t(&mut e, "SELECT pg_typeof(B'10' || B'11')::text"),
        "bit varying"
    );
    // No longer "unknown" for a plain bit literal either.
    assert_eq!(t(&mut e, "SELECT pg_typeof(B'101')::text"), "bit varying");
}

#[test]
fn bit_concat_projected_column_type_is_bit_varying() {
    // read01 A-bitcat — the *declared* column type a client sees for
    // `B'10' || B'11'` (RowDescription / QueryResult column) must be
    // `bit varying`, not the left operand's `bit`. PG's `||` is bitcat,
    // whose result is always varbit. Live-PG18.4: pg_typeof(B'10'||B'11')
    // = "bit varying".
    use spg_storage::DataType;
    let mut e = Engine::new();
    let QueryResult::Rows { columns, .. } = e.execute("SELECT B'10' || B'11'").unwrap() else {
        panic!("expected Rows");
    };
    assert_eq!(
        columns[0].ty,
        DataType::BitVarying(0),
        "bit concat projects a bit varying column, not {:?}",
        columns[0].ty
    );
}
