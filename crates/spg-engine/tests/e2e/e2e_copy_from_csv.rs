//! read01 A-group U9 (import side) — COPY … FROM stdin WITH (FORMAT
//! csv). The wire layer frames CopyData; this test drives the same
//! engine-side data path it uses — quote-aware record splitting
//! (csv_record_end) + field decoding (decode_copy_csv_record) +
//! build_copy_insert + execute — over a whole CSV buffer, then reads the
//! rows back. Round-trip behaviour asserted against live PostgreSQL 18.4
//! (empty-unquoted → NULL, "" → empty string, embedded delimiter /
//! newline inside quotes, doubled quote).

use spg_engine::copy::{build_copy_insert, csv_record_end, decode_copy_csv_record};
use spg_engine::{Engine, QueryResult};

/// Mirror the server's CSV branch of `process_copy_chunk`: split the
/// buffer into records with the quote-aware scanner, decode each, and
/// INSERT it. Returns the number of rows inserted.
fn import_csv(e: &mut Engine, table: &str, mut buf: Vec<u8>) -> u64 {
    // The wire layer appends a trailing newline at CopyDone; do the same.
    if !buf.is_empty() && !buf.ends_with(b"\n") {
        buf.push(b'\n');
    }
    let mut inserted = 0;
    while let Some(end) = csv_record_end(&buf, b',', b'"') {
        let record: Vec<u8> = buf.drain(..end).collect();
        let mut rec = &record[..record.len() - 1];
        if rec.last() == Some(&b'\r') {
            rec = &rec[..rec.len() - 1];
        }
        if rec.is_empty() {
            continue;
        }
        let row_text = std::str::from_utf8(rec).unwrap();
        let values = decode_copy_csv_record(row_text, ',', '"', "");
        let sql = build_copy_insert(table, None, &values);
        e.execute(&sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
        inserted += 1;
    }
    inserted
}

fn cell(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap() else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn csv_import_quoting_null_and_embedded_specials() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ci (id INT, a TEXT, b TEXT, c TEXT)")
        .unwrap();
    // Row 1: quoted field with an embedded comma + a doubled quote.
    // Row 2: an embedded newline inside a quoted field (spans lines);
    //        the third field is empty-unquoted → NULL.
    // Row 3: quoted empty "" → the empty string (distinct from NULL).
    let buf = b"1,\"x,y\",\"a\"\"b\",plain\n2,\"line\nbreak\",r,\n3,p,\"\",q\n".to_vec();
    assert_eq!(import_csv(&mut e, "ci", buf), 3);

    // Row 1 fields.
    assert_eq!(
        cell(&mut e, "SELECT a FROM ci WHERE id = 1"),
        spg_storage::Value::text("x,y")
    );
    assert_eq!(
        cell(&mut e, "SELECT b FROM ci WHERE id = 1"),
        spg_storage::Value::text("a\"b")
    );
    // Row 2: embedded newline preserved; third field NULL.
    assert_eq!(
        cell(&mut e, "SELECT a FROM ci WHERE id = 2"),
        spg_storage::Value::text("line\nbreak")
    );
    assert!(matches!(
        cell(&mut e, "SELECT c FROM ci WHERE id = 2"),
        spg_storage::Value::Null
    ));
    // Row 3: quoted empty string is NOT NULL.
    assert!(matches!(
        cell(&mut e, "SELECT (b IS NULL) FROM ci WHERE id = 3"),
        spg_storage::Value::Bool(false)
    ));
    assert_eq!(
        cell(&mut e, "SELECT b FROM ci WHERE id = 3"),
        spg_storage::Value::text("")
    );
}

#[test]
fn csv_import_crlf_line_endings() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cr (id INT, t TEXT)").unwrap();
    // CRLF terminators — the trailing \r is stripped, not stored.
    let buf = b"1,hello\r\n2,world\r\n".to_vec();
    assert_eq!(import_csv(&mut e, "cr", buf), 2);
    assert_eq!(
        cell(&mut e, "SELECT t FROM cr WHERE id = 1"),
        spg_storage::Value::text("hello")
    );
    assert_eq!(
        cell(&mut e, "SELECT t FROM cr WHERE id = 2"),
        spg_storage::Value::text("world")
    );
}

#[test]
fn csv_export_import_round_trip() {
    // A value with every CSV special char survives export → import.
    let mut e = Engine::new();
    e.execute("CREATE TABLE rt (id INT, t TEXT)").unwrap();
    e.execute("INSERT INTO rt VALUES (1, concat('a,b\"c', chr(10), 'd'))")
        .unwrap();
    let QueryResult::Rows { rows, .. } =
        e.execute("COPY rt TO STDOUT WITH (FORMAT csv)").unwrap()
    else {
        panic!("expected Rows");
    };
    let exported: Vec<u8> = rows
        .iter()
        .flat_map(|r| match &r.values[0] {
            spg_storage::Value::Text(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(b'\n');
                b
            }
            other => panic!("got {other:?}"),
        })
        .collect();
    e.execute("CREATE TABLE rt2 (id INT, t TEXT)").unwrap();
    assert_eq!(import_csv(&mut e, "rt2", exported), 1);
    assert_eq!(
        cell(&mut e, "SELECT t FROM rt2 WHERE id = 1"),
        spg_storage::Value::text("a,b\"c\nd")
    );
}
