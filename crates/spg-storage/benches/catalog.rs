// Bench code allow-list — see crates/spg-crypto/benches/hash.rs for rationale.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names
)]

//! Stone-level criterion bench for `spg-storage`. Measures the three
//! hot paths visible to the engine and server:
//!   * catalog serialize/deserialize roundtrip on a 100-row, 3-col table
//!   * HNSW index build over 200 vectors (dim=8)
//!   * HNSW top-1 search over a 200-vector built index

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use spg_storage::{
    Catalog, ColumnSchema, DataType, NSW_DEFAULT_M, NswMetric, Row, TableSchema, Value,
    VecEncoding, nsw_query,
};

fn build_catalog(n_rows: usize) -> Catalog {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "users",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("score", DataType::Float, true),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("users").unwrap();
    for i in 0..n_rows {
        t.insert(Row::new(vec![
            Value::Int(i32::try_from(i).unwrap()),
            Value::Text(format!("user-{i}")),
            Value::Float(f64::from(i as i32) * 0.1),
        ]))
        .unwrap();
    }
    cat
}

fn build_vector_catalog(n_rows: usize) -> Catalog {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 8,
                    encoding: VecEncoding::F32,
                },
                false,
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    for i in 0..n_rows {
        let f = i as f32;
        let v = vec![
            f * 0.01,
            f * 0.02,
            f * 0.03,
            f * 0.04,
            f * 0.05,
            f * 0.06,
            f * 0.07,
            f * 0.08,
        ];
        t.insert(Row::new(vec![
            Value::Int(i32::try_from(i).unwrap()),
            Value::Vector(v),
        ]))
        .unwrap();
    }
    t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
    cat
}

/// 50-table catalog with 1000 random lookups by name. Exercises the
/// v3.1.2 sidecar `by_name: BTreeMap<String, usize>` lookup index —
/// the win over the old `Vec<Table>` linear scan only shows when the
/// catalog actually has more than a handful of tables.
fn bench_catalog_multitable_lookup(c: &mut Criterion) {
    let mut cat = Catalog::new();
    for i in 0..50 {
        cat.create_table(TableSchema::new(
            format!("table_{i:02}"),
            vec![ColumnSchema::new("id", DataType::Int, false)],
        ))
        .unwrap();
    }
    // Cycle through every name many times so the bench reflects steady
    // state, not a one-off lookup.
    let names: Vec<String> = (0..50).map(|i| format!("table_{i:02}")).collect();
    c.bench_function("catalog_lookup_n50", |b| {
        b.iter(|| {
            for n in &names {
                let t = cat.get(black_box(n.as_str())).expect("present");
                black_box(t);
            }
        });
    });
}

fn bench_catalog_roundtrip(c: &mut Criterion) {
    let cat = build_catalog(100);
    let bytes = cat.serialize();
    c.bench_function("catalog_serialize_100rows", |b| {
        b.iter(|| {
            let out = cat.serialize();
            black_box(out);
        });
    });
    c.bench_function("catalog_deserialize_100rows", |b| {
        b.iter(|| {
            let restored = Catalog::deserialize(black_box(&bytes)).expect("deserialize ok");
            black_box(restored);
        });
    });
}

fn bench_hnsw_build(c: &mut Criterion) {
    c.bench_function("hnsw_build_200rows_dim8", |b| {
        b.iter_batched(
            || {
                // Build the table without the index yet — bench cost is
                // strictly the `add_nsw_index` bulk-build pass.
                let mut cat = Catalog::new();
                cat.create_table(TableSchema::new(
                    "vecs",
                    vec![
                        ColumnSchema::new("id", DataType::Int, false),
                        ColumnSchema::new(
                            "v",
                            DataType::Vector {
                                dim: 8,
                                encoding: VecEncoding::F32,
                            },
                            false,
                        ),
                    ],
                ))
                .unwrap();
                let t = cat.get_mut("vecs").unwrap();
                for i in 0..200 {
                    let f = i as f32;
                    t.insert(Row::new(vec![
                        Value::Int(i32::try_from(i).unwrap()),
                        Value::Vector(vec![
                            f * 0.01,
                            f * 0.02,
                            f * 0.03,
                            f * 0.04,
                            f * 0.05,
                            f * 0.06,
                            f * 0.07,
                            f * 0.08,
                        ]),
                    ]))
                    .unwrap();
                }
                cat
            },
            |mut cat| {
                let t = cat.get_mut("vecs").unwrap();
                t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
                black_box(cat);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_hnsw_search(c: &mut Criterion) {
    let cat = build_vector_catalog(200);
    let table = cat.get("vecs").expect("table present");
    let query = vec![1.5_f32, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0];
    c.bench_function("hnsw_search_top10_dim8_n200", |b| {
        b.iter(|| {
            let hits = nsw_query(
                black_box(table),
                "v_idx",
                black_box(&query),
                10,
                NswMetric::L2,
            );
            black_box(hits);
        });
    });
}

criterion_group!(
    benches,
    bench_catalog_roundtrip,
    bench_catalog_multitable_lookup,
    bench_hnsw_build,
    bench_hnsw_search
);
criterion_main!(benches);
