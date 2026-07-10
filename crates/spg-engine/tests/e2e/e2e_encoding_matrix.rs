//! v7.38 (read01, T30) — single-byte encoding transcode via convert_from /
//! convert_to for LATIN1/2/9 and KOI8-R/U (PG-sourced tables). Multibyte
//! encodings remain unsupported (honest error). Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Bool(b) => b.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn encoding_matrix() {
    let mut e = Engine::new();
    // Real per-encoding mappings (not a passthrough): 0xA9 is Š in LATIN2, © in
    // LATIN1; 0xA4 is € in LATIN9; 0xF0 is П in KOI8-R.
    assert_eq!(
        text(&mut e, r"SELECT convert_from('\xa9'::bytea, 'LATIN2')"),
        "Š"
    );
    assert_eq!(
        text(&mut e, r"SELECT convert_from('\xa9'::bytea, 'LATIN1')"),
        "©"
    );
    assert_eq!(
        text(&mut e, r"SELECT convert_from('\xa4'::bytea, 'LATIN9')"),
        "€"
    );
    assert_eq!(
        text(&mut e, r"SELECT convert_from('\xf0'::bytea, 'KOI8R')"),
        "П"
    );
    // Round-trips.
    assert_eq!(
        text(
            &mut e,
            r"SELECT convert_to(convert_from('\xc0c1'::bytea,'KOI8R'),'KOI8R') = '\xc0c1'::bytea"
        ),
        "true"
    );
    assert_eq!(
        text(&mut e, r"SELECT encode(convert_to('Š','LATIN2'),'hex')"),
        "a9"
    );
    // UTF8 / SQL_ASCII pass through; a multibyte encoding is a clear error.
    assert_eq!(
        text(&mut e, r"SELECT convert_from('hi'::bytea, 'UTF8')"),
        "hi"
    );
    assert!(
        e.execute(r"SELECT convert_from('\xff'::bytea, 'EUC_JP')")
            .is_err()
    );
}
