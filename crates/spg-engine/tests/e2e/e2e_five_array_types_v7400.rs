//! v7.40.0 — `real[]`, `time[]`, `timetz[]`, `inet[]`, `xml[]`.
//!
//! Five spellings PostgreSQL 18.6 accepts at `CREATE TABLE` and SPG
//! 7.39.13 refused at the type name. Unlike `oid[]` — whose storage
//! type already existed and only needed a parser arm — these five had
//! no array at all: no `Value` variant, no codec tag, no wire OID, no
//! text form.
//!
//! Every expectation below was read off PostgreSQL 18.6 before it was
//! written down, including the two the element type alone would not
//! have told you:
//!
//!   * `ARRAY[1.5::real, 2]` is `real[]` there, not `double
//!     precision[]` — only an actual `float8` in the list widens it.
//!   * `real`, `time`, `timetz` and `inet` elements are emitted BARE
//!     inside the braces; `xml` is quoted when it holds a comma,
//!     because an XML document can contain one.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, a real[], b time[], c timetz[], d inet[], e xml[])")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES \
         (1,'{1.5,NULL,2}','{12:34:56,NULL}','{12:34:56+02,NULL}','{10.0.0.1,NULL}','{<a/>,NULL}'), \
         (2,'{}','{}','{}','{}','{}'), \
         (3,NULL,NULL,NULL,NULL,NULL)",
    )
    .unwrap();
    e
}

#[test]
fn the_five_array_columns_read_back_in_pg_text_form() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT id,a,b,c,d,e FROM t ORDER BY id"),
        [
            "1|{1.5,NULL,2}|{12:34:56,NULL}|{12:34:56+02,NULL}|{10.0.0.1,NULL}|{<a/>,NULL}",
            "2|{}|{}|{}|{}|{}",
            "3|NULL|NULL|NULL|NULL|NULL",
        ]
    );
}

#[test]
fn the_five_array_columns_name_their_types() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT pg_typeof(a),pg_typeof(b),pg_typeof(c),pg_typeof(d),pg_typeof(e) \
             FROM t WHERE id=1"
        ),
        ["real[]|time without time zone[]|time with time zone[]|inet[]|xml[]"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 't'::regclass AND attnum > 1 ORDER BY attnum"
        ),
        [
            "real[]",
            "time without time zone[]",
            "time with time zone[]",
            "inet[]",
            "xml[]",
        ]
    );
    // PostgreSQL reports `ARRAY` here for every array column.
    assert_eq!(
        rows(
            &mut e,
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name='t' AND column_name<>'id' ORDER BY ordinal_position"
        ),
        ["ARRAY", "ARRAY", "ARRAY", "ARRAY", "ARRAY"]
    );
}

/// The element menu — indexing, cardinality, unnest — was written
/// variant by variant, so a new array variant reaches it only if each
/// site is extended. Values measured on PostgreSQL 18.6.
#[test]
fn the_five_arrays_index_count_and_unnest() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT a[1],b[1],c[1],d[1],e[1] FROM t WHERE id=1"),
        ["1.5|12:34:56|12:34:56+02|10.0.0.1|<a/>"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT cardinality(a),cardinality(b),array_length(d,1) FROM t WHERE id=1"
        ),
        ["3|2|2"]
    );
    assert_eq!(
        rows(&mut e, "SELECT unnest(b) FROM t WHERE id=1"),
        ["12:34:56", "NULL"]
    );
}

/// PostgreSQL 18.6, measured: an `int` or a `numeric` beside a `real`
/// keeps the array `real[]`; only a `float8` widens it. SPG answered
/// `double precision[]` for all three, because `Value::Real` had
/// nowhere else to go.
#[test]
fn a_real_beside_an_integer_stays_a_real_array() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT pg_typeof(ARRAY[1.5::real,2]), \
                    pg_typeof(ARRAY[1.5::real,2.5::numeric]), \
                    pg_typeof(ARRAY[1.5::real,2.0::float8]), \
                    pg_typeof(ARRAY['12:00'::time]), \
                    pg_typeof(ARRAY['10.0.0.1'::inet])"
        ),
        ["real[]|real[]|double precision[]|time without time zone[]|inet[]"]
    );
}

/// `array_agg` names its own result type, from a table that was a
/// second copy of the same element menu.
#[test]
fn array_agg_over_the_new_element_types() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (x time)").unwrap();
    e.execute("INSERT INTO s VALUES ('12:00'),('13:00')")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT array_agg(x), pg_typeof(array_agg(x)) FROM s"
        ),
        ["{12:00:00,13:00:00}|time without time zone[]"]
    );
}

/// An XML element holding a comma must come back quoted — the array
/// text form is ambiguous otherwise. PostgreSQL 18.6:
/// `{<a/>,"<b>x,y</b>",NULL}`.
#[test]
fn an_xml_element_with_a_comma_is_quoted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE x (c xml[])").unwrap();
    e.execute("INSERT INTO x VALUES ('{<a/>,\"<b>x,y</b>\",NULL}')")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT c FROM x"),
        ["{<a/>,\"<b>x,y</b>\",NULL}"]
    );
}

/// `pg_attribute.attndims` had its own shorter copy of the array-type
/// list and answered 0 for six spellings PostgreSQL 18.6 answers 1
/// for (measured: `varchar[]`, `char[]`, `money[]`, `interval[]`,
/// `jsonb[]`, `oid[]`). One table now serves both.
#[test]
fn every_array_column_reports_one_dimension() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE dims (a varchar(4)[], b char(2)[], c money[], d interval[], \
         e jsonb[], f oid[], g real[], h time[], i timetz[], j inet[], k xml[], m text)",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT attndims FROM pg_attribute WHERE attrelid='dims'::regclass \
             AND attnum > 0 ORDER BY attnum"
        ),
        ["1", "1", "1", "1", "1", "1", "1", "1", "1", "1", "1", "0"]
    );
}
