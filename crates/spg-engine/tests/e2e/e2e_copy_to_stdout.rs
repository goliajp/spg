//! COPY table [(cols)] TO STDOUT — text-format row stream as a
//! single-text-column result set.

use spg_engine::{Engine, QueryResult};

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, columns } = r else {
        panic!("expected Rows");
    };
    assert_eq!(columns.len(), 1);
    rows.iter()
        .map(|row| match &row.values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn copy_text_format_basics() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (id INT, t TEXT)").unwrap();
    e.execute("INSERT INTO ct VALUES (1, 'plain'), (2, NULL)")
        .unwrap();
    let got = lines(&mut e, "COPY ct TO STDOUT");
    assert_eq!(got, ["1\tplain", "2\t\\N"]);
}

#[test]
fn copy_escapes_and_column_list() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ce (id INT, t TEXT)").unwrap();
    // Tab and newline in the cell must escape; the column list
    // narrows and reorders.
    e.execute("INSERT INTO ce VALUES (1, concat('a', chr(9), 'b', chr(10), 'c'))")
        .unwrap();
    let got = lines(&mut e, "COPY ce (t, id) TO STDOUT");
    assert_eq!(got, ["a\\tb\\nc\t1"]);
    // Round-trip through the decoder recovers the original.
    let cells = spg_engine::copy::decode_copy_text_row(&got[0]);
    assert_eq!(cells[0].as_deref(), Some("a\tb\nc"));
    assert_eq!(cells[1].as_deref(), Some("1"));
}

#[test]
fn copy_options_and_files_error() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE co2 (id INT)").unwrap();
    assert!(e.execute("COPY co2 TO '/tmp/f.csv'").is_err());
    assert!(e.execute("COPY co2 TO STDOUT WITH (FORMAT csv)").is_err());
}
