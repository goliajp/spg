// Bench code allow-list — see crates/spg-crypto/benches/hash.rs for rationale.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names
)]

//! Stone-level criterion bench for `spg-wire`. Measures the three
//! frame shapes the server hot-loop touches every request:
//!   * `Query` encode/decode  (`build_query` → encode → decode → `parse_query`)
//!   * `RowDescription` roundtrip (3-column "id, name, score")
//!   * `DataRow` roundtrip (one row with 3 mixed-type `WireValue`s)

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use spg_wire::{
    ColumnDesc, WireType, WireValue, build_data_row, build_query, build_row_description, decode,
    encode, parse_data_row, parse_query, parse_row_description,
};

fn bench_query_roundtrip(c: &mut Criterion) {
    let sql = "SELECT id, name FROM users WHERE id = 42";
    c.bench_function("query_encode_decode_roundtrip", |b| {
        b.iter(|| {
            let frame = build_query(black_box(sql));
            let mut buf = Vec::with_capacity(64);
            encode(&frame, &mut buf).expect("encode ok");
            let (decoded, _) = decode(&buf).expect("decode ok");
            let _ = parse_query(black_box(&decoded));
        });
    });
}

fn bench_row_description(c: &mut Criterion) {
    let cols = vec![
        ColumnDesc {
            name: "id".into(),
            ty: WireType::Int,
            nullable: false,
        },
        ColumnDesc {
            name: "name".into(),
            ty: WireType::Text,
            nullable: false,
        },
        ColumnDesc {
            name: "score".into(),
            ty: WireType::Float,
            nullable: true,
        },
    ];
    c.bench_function("row_description_3col_roundtrip", |b| {
        b.iter(|| {
            let frame = build_row_description(black_box(&cols)).expect("build ok");
            let mut buf = Vec::with_capacity(64);
            encode(&frame, &mut buf).expect("encode ok");
            let (decoded, _) = decode(&buf).expect("decode ok");
            let _ = parse_row_description(&decoded).expect("parse ok");
        });
    });
}

fn bench_data_row(c: &mut Criterion) {
    let row = vec![
        WireValue::Int(42),
        WireValue::Text("alice".into()),
        WireValue::Float(95.5),
    ];
    c.bench_function("data_row_3col_roundtrip", |b| {
        b.iter(|| {
            let frame = build_data_row(black_box(&row)).expect("build ok");
            let mut buf = Vec::with_capacity(64);
            encode(&frame, &mut buf).expect("encode ok");
            let (decoded, _) = decode(&buf).expect("decode ok");
            let _ = parse_data_row(&decoded).expect("parse ok");
        });
    });
}

criterion_group!(
    benches,
    bench_query_roundtrip,
    bench_row_description,
    bench_data_row
);
criterion_main!(benches);
