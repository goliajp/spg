//! pgwire result encoding — `QueryResult` -> wire frames and the
//! `Value`/`DataType` -> `WireValue`/`WireType` conversions. Lifted
//! out of `main.rs` (server file split).

use std::io::Write;
use std::net::TcpStream;

use spg_engine::{EngineError, QueryResult};
use spg_storage::{ColumnSchema, DataType, Row, Value};
use spg_wire::{
    ColumnDesc, Frame, WireType, WireValue, build_command_complete, build_data_row,
    build_data_row_batch, build_error_response, build_row_description, encode,
};

use crate::BATCH_ROWS_PER_FRAME;

pub(crate) fn emit_result(
    stream: &mut TcpStream,
    result: Result<QueryResult, EngineError>,
) -> std::io::Result<()> {
    match result {
        Ok(QueryResult::CommandOk { affected, .. }) => {
            write_frame(stream, &build_command_complete(affected as u64))
        }
        Ok(QueryResult::Rows { columns, rows }) => {
            // v3.3.1: encode the entire response (RowDescription +
            // DataRowBatch chunks + CommandComplete) into one Vec<u8>
            // then a single write_all. Saves 2 syscalls per SELECT vs
            // the old 3-write_frame path.
            let descs = columns
                .iter()
                .map(column_schema_to_desc)
                .collect::<Vec<_>>();
            let rd =
                build_row_description(&descs).map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut out: Vec<u8> = Vec::with_capacity(
                spg_wire::FRAME_HEADER_LEN + rd.payload.len() + rows.len() * 64 + 16,
            );
            encode(&rd, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
            // v7.15.0 — column types thread through the wire path
            // so TIMESTAMPTZ can render with PG-canonical `+00`
            // offset, distinguishing it from plain TIMESTAMP.
            // Both are stored as i64 microseconds UTC; the type
            // tag is the only thing that says "include offset".
            let col_types: Vec<DataType> = columns.iter().map(|c| c.ty).collect();
            if rows.len() <= 1 {
                for row in rows {
                    let wire = row_to_wire_with_types(&row, &col_types);
                    let frame =
                        build_data_row(&wire).map_err(|e| std::io::Error::other(e.to_string()))?;
                    encode(&frame, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
                }
            } else {
                let wire_rows: Vec<Vec<WireValue>> = rows
                    .iter()
                    .map(|r| row_to_wire_with_types(r, &col_types))
                    .collect();
                for chunk in wire_rows.chunks(BATCH_ROWS_PER_FRAME) {
                    let frame = build_data_row_batch(chunk)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    encode(&frame, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
                }
            }
            let cc = build_command_complete(0);
            encode(&cc, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
            stream.write_all(&out)
        }
        Err(e) => write_frame(stream, &build_error_response(&e.to_string())),
        // v7.5.0 — QueryResult is #[non_exhaustive].
        Ok(_) => write_frame(
            stream,
            &build_error_response("unexpected QueryResult variant"),
        ),
    }
}

fn column_schema_to_desc(c: &ColumnSchema) -> ColumnDesc {
    ColumnDesc {
        name: c.name.clone(),
        ty: data_type_to_wire(c.ty),
        nullable: c.nullable,
    }
}

const fn data_type_to_wire(t: DataType) -> WireType {
    match t {
        // v1.11 surfaces SMALLINT as INT on the wire — the wire layer
        // doesn't (yet) carry a separate 16-bit tag, and PG drivers
        // happily render an i32 for any narrower integer column.
        DataType::SmallInt | DataType::Int => WireType::Int,
        DataType::BigInt => WireType::BigInt,
        DataType::Float => WireType::Float,
        DataType::Real => WireType::Float,
        // VARCHAR / CHAR / NUMERIC / DATE / TIMESTAMP collapse to
        // TEXT on the wire. Schema tracks bounds and precision; values
        // are plain UTF-8 in their canonical text forms.
        DataType::Text
        | DataType::Name
        | DataType::Varchar(_)
        | DataType::Char(_)
        | DataType::Numeric { .. }
        | DataType::Date
        | DataType::Timestamp
        | DataType::Timestamptz
        | DataType::Interval
        | DataType::Json
        | DataType::Jsonb
        // v7.10.4 — BYTEA serialises to Text on the wire as the
        // PG hex form (`\xDEADBEEF`). pg_type OID 17 is set
        // elsewhere via `pg_type_oid`; the WireType collapses
        // to the catch-all Text path so the existing encoder
        // emits the hex-formatted body.
        | DataType::Bytes
        // v7.10.9 — TEXT[] collapses to Text on the wire as the
        // PG external array form `{a,b,NULL}`. OID 1009 is set
        // via `pg_type_oid`. Binary array format lands in v7.12+.
        | DataType::TextArray
        // v7.11.12 — INT[] / BIGINT[] same text-mode collapse;
        // OIDs 1007 / 1016 via `pg_type_oid`.
        | DataType::IntArray
        | DataType::BigIntArray
        // v7.37.5 β-P4 — INTERVAL[] collapses to Text on the wire
        // as PG external array form: `{"1 day","24:00:00",NULL}`.
        // RowDescription advertises OID 1187 via `pg_type_oid`.
        | DataType::IntervalArray
        // v7.37.5 γ — array-of-scalar family on the wire as PG
        // external array text. RowDescription advertises the
        // family OIDs (1000/1005/1022/1231/1182/1115/1185/2951/
        // 199/3807/1001/1015/1014) via `pg_type_oid`.
        | DataType::BoolArray
        | DataType::SmallIntArray
        | DataType::FloatArray
        | DataType::NumericArray
        | DataType::DateArray
        | DataType::TimestampArray
        | DataType::TimestamptzArray
        | DataType::UuidArray
        | DataType::JsonArray
        | DataType::JsonbArray
        | DataType::BytesArray
        | DataType::VarcharArray
        | DataType::CharArray
        // v7.37.5 ε — geometry collapses to PG external text
        // forms ((x,y) / [(x1,y1),(x2,y2)] / etc). RowDescription
        // advertises 600/601/602/603/604/628/718.
        | DataType::Point
        | DataType::Lseg
        | DataType::Path
        | DataType::PgBox
        | DataType::Polygon
        | DataType::Line
        | DataType::Circle
        // v7.37.5 ζ-A — network / bit / xml / "char" / money[]
        // collapse to PG external text. RowDescription advertises
        // 869 / 650 / 829 / 774 / 1560 / 1562 / 142 / 18 / 791.
        | DataType::Inet
        | DataType::Cidr
        | DataType::Macaddr
        | DataType::Macaddr8
        | DataType::PgLsn
        | DataType::Bit(_)
        | DataType::BitVarying(_)
        | DataType::Xml
        | DataType::Char1
        | DataType::MoneyArray
        // v7.37.5 δ — multirange collapses to PG external text
        // form `{[a,b),[c,d)}` on the wire. RowDescription
        // advertises 4451/4537/4536/4533/4534/4535 via
        // pg_type_oid.
        | DataType::Multirange(_)
        // v7.12.0 — tsvector / tsquery collapse to Text on the
        // wire; OIDs 3614 / 3615 advertised via `pg_type_oid`.
        | DataType::TsVector
        | DataType::TsQuery
        // v7.17.0 — UUID collapses to Text on the wire as the
        // canonical 8-4-4-4-12 lowercase hyphenated form. PG
        // OID 2950 is advertised via `pg_type_oid`; binary
        // 16-byte format lands when binary-format clients
        // arrive.
        | DataType::Uuid
        // v7.17.0 Phase 3.P0-32 — TIME collapses to Text on the
        // wire as canonical `HH:MM:SS[.ffffff]`. PG OID 1083
        // advertised via `pg_type_oid`.
        | DataType::Time
        // v7.17.0 Phase 3.P0-33 — YEAR collapses to Text on the
        // wire as 4-digit zero-padded. Pgwire advertises as
        // INT4 OID; integer clients still parse it cleanly.
        | DataType::Year
        // v7.17.0 Phase 3.P0-34 — TIMETZ collapses to Text on
        // the wire as `HH:MM:SS[.ffffff]±HH[:MM]`. PG OID 1266
        // advertised via `pg_type_oid`.
        | DataType::TimeTz
        // v7.17.0 Phase 3.P0-35 — MONEY collapses to Text on
        // the wire as canonical `$N,NNN.CC`. PG OID 790
        // advertised via `pg_type_oid`.
        | DataType::Money
        // v7.17.0 Phase 3.P0-38 — range types collapse to Text
        // on the wire as canonical `[a,b)` / `(a,b]` form. PG
        // OIDs (3904/3926/...) advertised via `pg_type_oid`.
        | DataType::Range(_)
        // v7.17.0 Phase 3.P0-39 — hstore collapses to Text on
        // the wire as canonical `"k"=>"v"` form.
        | DataType::Hstore
        // v7.17.0 Phase 3.P0-40 — 2D arrays collapse to Text on
        // the wire as nested `'{{a,b},{c,d}}'` form.
        | DataType::IntArray2D
        | DataType::BigIntArray2D
        | DataType::TextArray2D => WireType::Text,
        | DataType::BoolArray2D => WireType::Text,
        DataType::Bool => WireType::Bool,
        // RowDescription drops the dimension; DataRow's WireValue::Vector
        // carries the actual element count back to the client.
        DataType::Vector { .. } => WireType::Vector,
    }
}

/// v7.15.0 — column-type-aware wire conversion. Used wherever a
/// SELECT result needs to distinguish TIMESTAMP from TIMESTAMPTZ
/// at render time (canonical PG output includes `+00` for
/// TIMESTAMPTZ). Both share the i64 microseconds UTC on-disk
/// shape; the type-tag is what flips the renderer.
fn row_to_wire_with_types(r: &Row, col_types: &[DataType]) -> Vec<WireValue> {
    r.values
        .iter()
        .zip(col_types.iter())
        .map(|(v, ty)| value_to_wire_typed(v, *ty))
        .collect()
}

fn value_to_wire_typed(v: &Value, ty: DataType) -> WireValue {
    match (v, ty) {
        (Value::Timestamp(t), DataType::Timestamptz) => {
            WireValue::Text(spg_engine::eval::format_timestamptz(*t))
        }
        _ => value_to_wire(v),
    }
}

fn value_to_wire(v: &Value) -> WireValue {
    match v {
        Value::Null => WireValue::Null,
        // SMALLINT widens to wire INT — drivers see a plain i32.
        Value::SmallInt(n) => WireValue::Int(i32::from(*n)),
        Value::Int(n) => WireValue::Int(*n),
        Value::BigInt(n) => WireValue::BigInt(*n),
        Value::Float(x) => WireValue::Float(*x),
        // v4.9: TEXT and JSON ride the wire identically — the
        // client's column type (RowDescription OID) carries the
        // "this is JSON" semantic.
        Value::Text(s) | Value::Json(s) => WireValue::Text(s.to_string()),
        Value::Bool(b) => WireValue::Bool(*b),
        Value::Vector(v) => WireValue::Vector(v.to_vec()),
        // v6.0.1: SQ8 cells dequantise to f32 on the wire so
        // pgwire clients (psql, drivers, the conformance corpora)
        // see the same `WireValue::Vector` shape regardless of
        // the column's storage encoding. Recall envelope absorbs
        // the ≤ (max-min)/255/2 dequantisation error.
        Value::Sq8Vector(q) => WireValue::Vector(spg_storage::quantize::dequantize(q)),
        // v6.0.3: HalfVector cells decode bit-exactly back to f32.
        Value::HalfVector(h) => WireValue::Vector(h.to_f32_vec()),
        // NUMERIC / DATE / TIMESTAMP render as their canonical
        // text form on the wire. Drivers receive plain UTF-8,
        // identical to what `value_to_text` produces in the engine.
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => WireValue::Text(spg_engine::eval::format_numeric_kind(
            *kind, *scaled, *scale,
        )),
        Value::Date(d) => WireValue::Text(spg_engine::eval::format_date(*d)),
        Value::Timestamp(t) => WireValue::Text(spg_engine::eval::format_timestamp(*t)),
        Value::Interval {
            months,
            days,
            micros,
        } => WireValue::Text(spg_engine::eval::format_interval(*months, *days, *micros)),
        // v7.10.4 — BYTEA goes on the wire as PG hex text
        // (`\x` + lowercase hex). RowDescription advertises OID 17
        // (`pg_type_oid` covers that), and PG text-mode clients
        // expect this exact format. Binary-mode (sqlx/pgx Bind)
        // path lands when the wire layer grows BYTEA binary
        // codec — for now binary fetches surface as the hex text
        // and clients decode normally.
        Value::Bytes(b) => WireValue::Text(spg_engine::eval::format_bytea_hex(b)),
        // v7.10.9 — TEXT[] goes on the wire as PG external array
        // form (`{a,b,NULL}`). RowDescription advertises OID 1009.
        Value::TextArray(items) => WireValue::Text(spg_engine::eval::format_text_array(items)),
        // v7.11.14 — INT[] / BIGINT[] external form `{1,2,NULL}`.
        // RowDescription advertises OIDs 1007 / 1016.
        Value::IntArray(items) => WireValue::Text(spg_engine::eval::format_int_array(items)),
        Value::BigIntArray(items) => WireValue::Text(spg_engine::eval::format_bigint_array(items)),
        // v7.37.5 β-P4 — INTERVAL[] external array form.
        Value::IntervalArray(items) => {
            WireValue::Text(spg_engine::eval::format_interval_array(items))
        }
        // v7.37.5 γ — array-of-scalar external array text. Each
        // family has its own formatter so element rendering
        // matches the scalar's `value_to_text` (e.g. UUID lowercase
        // 8-4-4-4-12; BOOL `t`/`f`).
        Value::BoolArray(items) => WireValue::Text(spg_engine::eval::format_bool_array(items)),
        Value::SmallIntArray(items) => {
            WireValue::Text(spg_engine::eval::format_smallint_array(items))
        }
        Value::FloatArray(items) => WireValue::Text(spg_engine::eval::format_float_array(items)),
        Value::NumericArray(items) => {
            WireValue::Text(spg_engine::eval::format_numeric_array(items))
        }
        Value::DateArray(items) => WireValue::Text(spg_engine::eval::format_date_array(items)),
        Value::TimestampArray(items) => {
            WireValue::Text(spg_engine::eval::format_timestamp_array(items, false))
        }
        Value::TimestamptzArray(items) => {
            WireValue::Text(spg_engine::eval::format_timestamp_array(items, true))
        }
        Value::UuidArray(items) => WireValue::Text(spg_engine::eval::format_uuid_array(items)),
        // JSON / JSONB / VARCHAR / CHAR / BYTEA all share the
        // text-element array shape (per-element formatting is
        // identical for the wire / PG external array form).
        Value::JsonArray(items)
        | Value::JsonbArray(items)
        | Value::VarcharArray(items)
        | Value::CharArray(items) => WireValue::Text(spg_engine::eval::format_text_array(items)),
        Value::BytesArray(items) => WireValue::Text(spg_engine::eval::format_bytea_array(items)),
        // v7.37.5 δ — Multirange external form.
        Value::Multirange { kind: _, ranges } => {
            WireValue::Text(spg_engine::format_multirange(ranges))
        }
        // v7.37.5 ε — geometry external forms.
        Value::Point(p) => WireValue::Text(spg_engine::format_point(*p)),
        Value::Lseg(p1, p2) => WireValue::Text(spg_engine::format_lseg(*p1, *p2)),
        Value::PgBox(ur, ll) => WireValue::Text(spg_engine::format_pg_box(*ur, *ll)),
        Value::Line { a, b, c } => WireValue::Text(spg_engine::format_line(*a, *b, *c)),
        Value::Circle { center, radius } => {
            WireValue::Text(spg_engine::format_circle(*center, *radius))
        }
        Value::Path { points, closed } => WireValue::Text(spg_engine::format_path(points, *closed)),
        Value::Polygon(points) => WireValue::Text(spg_engine::format_polygon(points)),
        // v7.37.5 ζ-A — network / bit / xml / "char" / money[].
        Value::Inet { family, bits, addr } => {
            WireValue::Text(spg_engine::format_inet(*family, *bits, addr))
        }
        Value::Cidr { family, bits, addr } => {
            WireValue::Text(spg_engine::format_inet(*family, *bits, addr))
        }
        Value::Macaddr(m) => WireValue::Text(spg_engine::format_macaddr(m)),
        Value::Macaddr8(m) => WireValue::Text(spg_engine::format_macaddr8(m)),
        Value::PgLsn(l) => WireValue::Text(spg_engine::format_pg_lsn(*l)),
        Value::RegClass(_, name) => WireValue::Text(name.to_string()),
        Value::BitString { nbits, bytes } => {
            WireValue::Text(spg_engine::format_bit_string(*nbits, bytes))
        }
        // XML rides the wire as raw text — clients use OID 142 in
        // RowDescription to decode.
        Value::Xml(s) => WireValue::Text(s.to_string()),
        // `"char"` is a single byte; render as one-char string
        // (matches PG: `'A'::"char"` renders `A`).
        Value::Char1(b) => WireValue::Text((*b as char).to_string()),
        Value::MoneyArray(items) => WireValue::Text(spg_engine::eval::format_money_array(items)),
        // v7.12.0 — tsvector / tsquery on the wire as PG external
        // form. RowDescription OIDs 3614 / 3615 via `pg_type_oid`.
        Value::TsVector(lexs) => WireValue::Text(spg_engine::eval::format_tsvector(lexs)),
        Value::TsQuery(ast) => WireValue::Text(spg_engine::eval::format_tsquery(ast)),
        // v7.5.0 — Value is #[non_exhaustive].
        _ => WireValue::Text(format!("{v:?}")),
    }
}

pub(crate) fn write_frame(stream: &mut TcpStream, frame: &Frame) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(32);
    encode(frame, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&out)
}
