//! JSON / JSONB PG18 differential corpus (4th differential sweep).
//!
//! Ground truth captured from live PostgreSQL 18.4 on 2026-07-04.
//! Each `check` asserts SPG's output against the value observed.
//!
//! Two documented SEMANTIC representation choices mean SPG's jsonb
//! text form is NOT byte-identical to PG's and this is intentional
//! (see the DIVERGENCE notes below), so those cases assert SPG's own
//! form with the PG value recorded in a comment:
//!   * SPG renders jsonb compactly (no space after `:` / `,`);
//!     PG inserts a space.
//!   * SPG preserves object key insertion order; PG sorts keys and
//!     de-dups keeping the LAST value.
//!   * SPG preserves the original numeric lexeme (`1e2`, `1.0`);
//!     PG canonicalises numbers (`1e2` -> `100`).
//!
//! The operator/containment cases below WERE bugs, fixed in the
//! accompanying commit and locked here against a live-PG differential.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn cell(eng: &mut Engine, sql: &str) -> String {
    match eng.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            if rows.is_empty() {
                return "<NOROWS>".into();
            }
            let r = &rows[0].values;
            render(&r[r.len() - 1])
        }
        Ok(other) => format!("<NONROWS:{other:?}>"),
        Err(e) => format!("<ERR:{e:?}>"),
    }
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "<NULL>".into(),
        Value::Bool(b) => if *b { "t" } else { "f" }.into(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Json(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

fn check(eng: &mut Engine, sql: &str, expect: &str) {
    let got = cell(eng, sql);
    assert_eq!(got, expect, "\n  SQL: {sql}\n  want: {expect}\n  got:  {got}");
}

// ---- operators: -> ->> #> #>> (already correct; locked) ----
#[test]
fn jsonb_accessor_operators() {
    let mut e = Engine::new();
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2}'::jsonb -> 'a')::text", "1");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb -> 'x')::text", "<NULL>");
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2}'::jsonb ->> 'a')", "1");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb ->> 'x')", "<NULL>");
    check(&mut e, "SELECT ('[10,20,30]'::jsonb -> 0)::text", "10");
    check(&mut e, "SELECT ('[10,20,30]'::jsonb -> -1)::text", "30");
    check(&mut e, "SELECT ('[10,20,30]'::jsonb -> 9)::text", "<NULL>");
    check(&mut e, "SELECT ('[10,20,30]'::jsonb ->> 2)", "30");
    check(&mut e, "SELECT ('{\"a\":{\"b\":{\"c\":5}}}'::jsonb #> '{a,b,c}')::text", "5");
    check(&mut e, "SELECT ('{\"a\":{\"b\":{\"c\":5}}}'::jsonb #>> '{a,b,c}')", "5");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb #> '{a,b}')::text", "<NULL>");
}

// ---- containment / key-exists ----
#[test]
fn jsonb_containment_and_exists() {
    let mut e = Engine::new();
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2}'::jsonb @> '{\"a\":1}')", "t");
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2}'::jsonb @> '{\"a\":9}')", "f");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb <@ '{\"a\":1,\"b\":2}')", "t");
    check(&mut e, "SELECT ('[1,2,3]'::jsonb @> '[1,2]')", "t");
    check(&mut e, "SELECT ('[1,2,3]'::jsonb @> '[3,1]')", "t");
    check(&mut e, "SELECT ('{\"a\":{\"b\":1,\"c\":2}}'::jsonb @> '{\"a\":{\"b\":1}}')", "t");
    check(&mut e, "SELECT ('\"foo\"'::jsonb @> '\"foo\"')", "t");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb @> '{}')", "t");
    check(&mut e, "SELECT ('[1,2]'::jsonb @> '[]')", "t");
    // FIXED: top-level array @> scalar (PG special case). Was `f`.
    check(&mut e, "SELECT ('[1,2,3]'::jsonb @> '2')", "t");
    check(&mut e, "SELECT ('[1,2,3]'::jsonb @> '5')", "f");
    check(&mut e, "SELECT ('[1,2,3]'::jsonb @> '\"a\"')", "f");
    // ...but NOT recursively (must stay false, PG semantics).
    check(&mut e, "SELECT ('[1,[2,3]]'::jsonb @> '2')", "f");
    check(&mut e, "SELECT ('[1,[2,3]]'::jsonb @> '[2,3]')", "f");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb @> '1')", "f");
    check(&mut e, "SELECT ('[{\"a\":1}]'::jsonb @> '{\"a\":1}')", "f");
    check(&mut e, "SELECT ('[{\"a\":1,\"b\":2}]'::jsonb @> '[{\"a\":1}]')", "t");
    // <@ scalar contained by array (routes through same fix).
    check(&mut e, "SELECT ('2'::jsonb <@ '[1,2,3]')", "t");
    // key-exists family
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2}'::jsonb ? 'a')", "t");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb ? 'x')", "f");
    check(&mut e, "SELECT ('[\"a\",\"b\",\"c\"]'::jsonb ? 'b')", "t");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb ?| array['x','a'])", "t");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb ?| array['x','y'])", "f");
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2}'::jsonb ?& array['a','b'])", "t");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb ?& array['a','b'])", "f");
}

// ---- FIXED: = / <> equality operator (was a type error) ----
// Ground truth captured from live PostgreSQL 18.4 on 2026-07-04.
// jsonb equality is structural: object keys compared order-FREE,
// array elements order-SENSITIVE, numbers by value. Before the fix
// `jsonb = jsonb` raised TypeMismatch (no Value::Json arm in compare).
#[test]
fn jsonb_equality_operator() {
    let mut e = Engine::new();
    // identical -> true
    check(&mut e, "SELECT '{\"a\":1}'::jsonb = '{\"a\":1}'::jsonb", "t");
    // key-order-independent -> PG true (the headline fix)
    check(&mut e, "SELECT '{\"a\":1,\"b\":2}'::jsonb = '{\"b\":2,\"a\":1}'::jsonb", "t");
    // unequal value -> false
    check(&mut e, "SELECT '{\"a\":1}'::jsonb = '{\"a\":2}'::jsonb", "f");
    // <> negation
    check(&mut e, "SELECT '{\"a\":1}'::jsonb <> '{\"a\":2}'::jsonb", "t");
    check(&mut e, "SELECT '{\"a\":1}'::jsonb <> '{\"a\":1}'::jsonb", "f");
    // nested objects, keys shuffled at depth -> true
    check(&mut e, "SELECT '{\"a\":{\"x\":1,\"y\":2}}'::jsonb = '{\"a\":{\"y\":2,\"x\":1}}'::jsonb", "t");
    // arrays are ORDERED: reversed -> false
    check(&mut e, "SELECT '[1,2,3]'::jsonb = '[3,2,1]'::jsonb", "f");
    check(&mut e, "SELECT '[1,2,3]'::jsonb = '[1,2,3]'::jsonb", "t");
    // scalars
    check(&mut e, "SELECT '\"foo\"'::jsonb = '\"foo\"'::jsonb", "t");
    check(&mut e, "SELECT 'true'::jsonb = 'true'::jsonb", "t");
    check(&mut e, "SELECT 'null'::jsonb = 'null'::jsonb", "t");
    check(&mut e, "SELECT '5'::jsonb = '5'::jsonb", "t");
    // different top-level types -> false (object vs array)
    check(&mut e, "SELECT '{}'::jsonb = '[]'::jsonb", "f");
    // through real jsonb columns
    e.execute("CREATE TABLE je (id INT NOT NULL, o JSONB)").unwrap();
    e.execute("INSERT INTO je VALUES (1, '{\"a\":1,\"b\":2}')").unwrap();
    check(&mut e, "SELECT o = '{\"b\":2,\"a\":1}'::jsonb FROM je", "t");
    check(&mut e, "SELECT o = '{\"a\":9}'::jsonb FROM je", "f");
    // numeric = still works (compare arm untouched for non-Json)
    check(&mut e, "SELECT 1 = 1", "t");
    check(&mut e, "SELECT 'ab' = 'ab'", "t");

    // Duplicate object keys now collapse last-wins on the `::jsonb`
    // cast, so `{"a":1,"a":2}` canonicalises to `{"a":2}` and the
    // equality matches PG 18.4 (t).
    check(&mut e, "SELECT '{\"a\":1,\"a\":2}'::jsonb = '{\"a\":2}'::jsonb", "t");
    // jsonb numbers compare by value now (PG 18.4): 1 == 1.0 == 1e0,
    // 1.50 == 1.5, 1e3 == 1000.00; a real difference still false.
    check(&mut e, "SELECT '1'::jsonb = '1.0'::jsonb", "t");
    check(&mut e, "SELECT '1.50'::jsonb = '1.5'::jsonb", "t");
    check(&mut e, "SELECT '1e3'::jsonb = '1000.00'::jsonb", "t");
    check(&mut e, "SELECT '{\"a\":1}'::jsonb = '{\"a\":1.0}'::jsonb", "t");
    check(&mut e, "SELECT '1.5'::jsonb = '1.6'::jsonb", "f");
    // Containment routes through the same value comparison.
    check(&mut e, "SELECT '[1,2]'::jsonb @> '[1.0]'::jsonb", "t");
    // NOTE (ordering deferred): PG defines a total order on jsonb so
    // `'1'::jsonb < '2'::jsonb` is PG-TRUE, but SPG's compare wires
    // only = / <> for jsonb; the ordering operators stay a type error.
}

// ---- FIXED: || concat operator (was string concatenation) ----
#[test]
fn jsonb_concat_operator() {
    let mut e = Engine::new();
    // PG: {"a": 1, "b": 2}
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb || '{\"b\":2}'::jsonb)::text", "{\"a\":1,\"b\":2}");
    // PG: {"a": 1, "b": 9} — right wins on dup key
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2}'::jsonb || '{\"b\":9}'::jsonb)::text", "{\"a\":1,\"b\":9}");
    check(&mut e, "SELECT ('[1,2]'::jsonb || '[3,4]'::jsonb)::text", "[1,2,3,4]");
    check(&mut e, "SELECT ('[1,2]'::jsonb || '3'::jsonb)::text", "[1,2,3]");
    check(&mut e, "SELECT ('[1,2]'::jsonb || '{\"a\":1}'::jsonb)::text", "[1,2,{\"a\":1}]");
    // still works through a real jsonb column
    e.execute("CREATE TABLE jc (id INT NOT NULL, o JSONB, a JSONB)").unwrap();
    e.execute("INSERT INTO jc VALUES (1, '{\"a\":1,\"b\":2,\"c\":3}', '[10,20,30]')").unwrap();
    check(&mut e, "SELECT (o || '{\"d\":9}'::jsonb)::text FROM jc", "{\"a\":1,\"b\":2,\"c\":3,\"d\":9}");
    check(&mut e, "SELECT (a || '[40]'::jsonb)::text FROM jc", "[10,20,30,40]");
    // text || text still concatenates (must NOT be affected)
    check(&mut e, "SELECT 'ab' || 'cd'", "abcd");
}

// ---- FIXED: - delete operator (was type error) ----
#[test]
fn jsonb_delete_operator() {
    let mut e = Engine::new();
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2}'::jsonb - 'a')::text", "{\"b\":2}");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb - 'x')::text", "{\"a\":1}");
    check(&mut e, "SELECT ('[10,20,30]'::jsonb - 1)::text", "[10,30]");
    check(&mut e, "SELECT ('[10,20,30]'::jsonb - -1)::text", "[10,20]");
    // jsonb - text[] (multi-key delete)
    check(&mut e, "SELECT ('{\"a\":1,\"b\":2,\"c\":3}'::jsonb - array['a','c'])::text", "{\"b\":2}");
    // numeric - still works (must NOT be affected)
    check(&mut e, "SELECT 5 - 3", "2");
    // via column
    e.execute("CREATE TABLE jd (id INT NOT NULL, o JSONB, a JSONB)").unwrap();
    e.execute("INSERT INTO jd VALUES (1, '{\"a\":1,\"b\":2,\"c\":3}', '[10,20,30]')").unwrap();
    check(&mut e, "SELECT (o - 'a')::text FROM jd", "{\"b\":2,\"c\":3}");
    check(&mut e, "SELECT (a - 1)::text FROM jd", "[10,30]");
}

// ---- FIXED: #- delete-path operator (was unrecognised token) ----
#[test]
fn jsonb_delete_path_operator() {
    let mut e = Engine::new();
    check(&mut e, "SELECT ('{\"a\":{\"b\":1,\"c\":2}}'::jsonb #- '{a,b}')::text", "{\"a\":{\"c\":2}}");
    check(&mut e, "SELECT ('{\"a\":[1,2,3]}'::jsonb #- '{a,1}')::text", "{\"a\":[1,3]}");
    // via column
    e.execute("CREATE TABLE jp (id INT NOT NULL, o JSONB)").unwrap();
    e.execute("INSERT INTO jp VALUES (1, '{\"a\":{\"b\":1},\"z\":9}')").unwrap();
    check(&mut e, "SELECT (o #- '{a,b}')::text FROM jp", "{\"a\":{},\"z\":9}");
    // bare # (integer XOR) must NOT be affected
    check(&mut e, "SELECT 5 # 3", "6");
}

// ---- functions ----
#[test]
fn jsonb_functions() {
    let mut e = Engine::new();
    check(&mut e, "SELECT jsonb_build_object('a',1,'b',2)::text", "{\"a\":1,\"b\":2}");
    // DIVERGENCE (key order): SPG preserves insertion order -> {"b":1,"a":2};
    // PG sorts -> {"a": 2, "b": 1}. Documented representation choice.
    check(&mut e, "SELECT jsonb_build_object('b',1,'a',2)::text", "{\"b\":1,\"a\":2}");
    check(&mut e, "SELECT jsonb_build_array(1,'x',true,null)::text", "[1,\"x\",true,null]");
    check(&mut e, "SELECT to_jsonb(5)::text", "5");
    check(&mut e, "SELECT to_jsonb('hi'::text)::text", "\"hi\"");
    check(&mut e, "SELECT jsonb_set('{\"a\":1,\"b\":2}','{a}','5')::text", "{\"a\":5,\"b\":2}");
    check(&mut e, "SELECT jsonb_set('{\"a\":{\"b\":1}}','{a,b}','9')::text", "{\"a\":{\"b\":9}}");
    check(&mut e, "SELECT jsonb_set('{\"a\":1}','{c}','3')::text", "{\"a\":1,\"c\":3}");
    check(&mut e, "SELECT jsonb_set('{\"a\":1}','{c}','3',true)::text", "{\"a\":1,\"c\":3}");
    check(&mut e, "SELECT jsonb_set('{\"a\":1}','{c}','3',false)::text", "{\"a\":1}");
    check(&mut e, "SELECT jsonb_insert('{\"a\":[1,2,3]}','{a,1}','9')::text", "{\"a\":[1,9,2,3]}");
    check(&mut e, "SELECT jsonb_insert('{\"a\":[1,2,3]}','{a,1}','9',true)::text", "{\"a\":[1,2,9,3]}");
    check(&mut e, "SELECT jsonb_extract_path('{\"a\":{\"b\":5}}','a','b')::text", "5");
    check(&mut e, "SELECT jsonb_extract_path_text('{\"a\":{\"b\":5}}','a','b')", "5");
    // typeof / array_length via ::jsonb-cast argument
    check(&mut e, "SELECT jsonb_typeof('{\"a\":1}'::jsonb)", "object");
    check(&mut e, "SELECT jsonb_typeof('[1,2]'::jsonb)", "array");
    check(&mut e, "SELECT jsonb_typeof('\"x\"'::jsonb)", "string");
    check(&mut e, "SELECT jsonb_typeof('5'::jsonb)", "number");
    check(&mut e, "SELECT jsonb_typeof('true'::jsonb)", "boolean");
    check(&mut e, "SELECT jsonb_typeof('null'::jsonb)", "null");
    check(&mut e, "SELECT jsonb_array_length('[1,2,3,4]'::jsonb)", "4");
    check(&mut e, "SELECT jsonb_array_length('[]'::jsonb)", "0");
    check(&mut e, "SELECT jsonb_strip_nulls('{\"a\":null,\"b\":1}'::jsonb)::text", "{\"b\":1}");
    check(&mut e, "SELECT jsonb_path_exists('{\"a\":1}','$.a')", "t");
    check(&mut e, "SELECT jsonb_path_exists('{\"a\":1}','$.b')", "f");
    check(&mut e, "SELECT jsonb_agg(x)::text FROM (VALUES(1),(2),(3)) v(x)", "[1, 2, 3]");
    check(&mut e, "SELECT jsonb_object_agg(k,v)::text FROM (VALUES('a',1),('b',2)) v(k,v)", "{\"a\": 1, \"b\": 2}");
}

// ---- null semantics ----
#[test]
fn jsonb_null_semantics() {
    let mut e = Engine::new();
    check(&mut e, "SELECT jsonb_typeof('null'::jsonb)", "null");
    // JSON null -> ->> yields SQL NULL
    check(&mut e, "SELECT ('{\"a\":null}'::jsonb ->> 'a')", "<NULL>");
    check(&mut e, "SELECT ('{\"a\":null}'::jsonb ->> 'a') IS NULL", "t");
    check(&mut e, "SELECT ('{\"a\":1}'::jsonb -> 'x') IS NULL", "t");
    check(&mut e, "SELECT jsonb_strip_nulls('{\"a\":null}'::jsonb)::text", "{}");
    // DIVERGENCE: `('{"a":null}'::jsonb -> 'a')::text` — PG yields the
    // json string "null"; SPG yields SQL NULL here. Recorded, deferred
    // (SEMANTIC: distinguishing JSON-null from SQL-NULL through -> +
    // ::text needs a jsonb-typed intermediate SPG does not carry).
}

// ---- rendering / normalization (SEMANTIC representation choices) ----
#[test]
fn jsonb_rendering_divergences() {
    let mut e = Engine::new();
    check(&mut e, "SELECT '{}'::jsonb::text", "{}");
    check(&mut e, "SELECT '[]'::jsonb::text", "[]");
    check(&mut e, "SELECT '\"A\"'::jsonb::text", "\"A\"");
    // jsonb now canonicalises like PG 18.4: `, ` / `: ` whitespace,
    // keys sorted (length, then bytes) + dups collapsed last-wins,
    // numbers normalised. Non-ASCII stays verbatim UTF-8.
    check(&mut e, "SELECT '{\"k\":\"é\"}'::jsonb::text", "{\"k\": \"é\"}");
    check(&mut e, "SELECT '100000000000000000000'::jsonb::text", "100000000000000000000");
    check(&mut e, "SELECT '{\"n\":-0.5}'::jsonb::text", "{\"n\": -0.5}");
    check(&mut e, "SELECT '{\"b\":1,\"a\":2}'::jsonb::text", "{\"a\": 2, \"b\": 1}");
    check(&mut e, "SELECT '{\"a\":1,\"a\":2}'::jsonb::text", "{\"a\": 2}");
    check(&mut e, "SELECT '{\"n\":1e2}'::jsonb::text", "{\"n\": 100}");
    check(&mut e, "SELECT '{\"n\":1.0}'::jsonb::text", "{\"n\": 1.0}");
    // json (non-b) still preserves the input verbatim — no canonicalise.
    check(&mut e, "SELECT '{\"b\":1,\"a\":2}'::json::text", "{\"b\":1,\"a\":2}");
    check(&mut e, "SELECT '{\"a\":1,\"a\":2}'::json::text", "{\"a\":1,\"a\":2}");
}
