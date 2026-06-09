//! v7.17.0 Phase 3.P0-68 — sqlx bridge for pgvector `VECTOR(N)`
//! and PG `TSVECTOR`.
//!
//! `Vec<f32>` bridges the dense pgvector surface (any storage
//! encoding: default f32 / `USING SQ8` / `USING HALF` — quantised
//! variants dequantise to f32 at the adapter boundary).
//!
//! `String` Decode on TsVector / Vector cells falls through to
//! the canonical PG / pgvector external form, mirroring what
//! pgwire ships on the wire. The text path in `types/text.rs`
//! calls into the `try_*_as_string` helpers below.
//!
//! TsVector Encode is intentionally not implemented: clients
//! build a tsvector via `to_tsvector(...)` in SQL, not by
//! binding a raw lexeme list.

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::Value as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

// ---- text fallthrough helpers --------------------------------

/// Try to render a Vector cell into pgvector's canonical
/// external form (`[1, 2.5, -3]`). Returns `None` for non-
/// vector cells so the caller can keep trying.
pub(crate) fn try_vector_as_string(value: &EngineValue) -> Option<String> {
    let v = match value {
        EngineValue::Vector(v) => v.clone(),
        EngineValue::Sq8Vector(q) => spg_storage::quantize::dequantize(q),
        EngineValue::HalfVector(h) => h.to_f32_vec(),
        _ => return None,
    };
    Some(format_vector(&v))
}

/// Try to render a TsVector cell into PG's canonical external
/// form. Returns `None` for non-tsvector cells.
pub(crate) fn try_tsvector_as_string(value: &EngineValue) -> Option<String> {
    match value {
        EngineValue::TsVector(lex) => Some(format_tsvector(lex)),
        _ => None,
    }
}

/// pgvector canonical external form. Matches what the engine's
/// pgwire path sends so the adapter and wire agree.
fn format_vector(v: &[f32]) -> String {
    let mut out = String::with_capacity(v.len() * 6);
    out.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        format_f32_into(&mut out, *x);
    }
    out.push(']');
    out
}

fn format_f32_into(out: &mut String, x: f32) {
    use core::fmt::Write;
    // pgvector renders compact decimals: `1`, `2.5`, `-3` —
    // integer values drop the trailing `.0`, fractions print
    // their shortest round-trip form. f32 Display already does
    // that on stable Rust.
    let _ = write!(out, "{x}");
}

/// PG `tsvector` external form: `'word1':1 'word2':2,3A`.
/// Mirrors `spg_engine::eval::format_tsvector` exactly so the
/// adapter and pgwire agree byte-for-byte.
fn format_tsvector(lexs: &[spg_storage::TsLexeme]) -> String {
    let mut out = String::with_capacity(lexs.len() * 12);
    for (i, l) in lexs.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('\'');
        for c in l.word.chars() {
            if c == '\'' {
                out.push('\'');
            }
            out.push(c);
        }
        out.push('\'');
        if !l.positions.is_empty() {
            for (pi, p) in l.positions.iter().enumerate() {
                out.push(if pi == 0 { ':' } else { ',' });
                let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{p}"));
            }
            match l.weight {
                3 => out.push('A'),
                2 => out.push('B'),
                1 => out.push('C'),
                _ => {}
            }
        }
    }
    out
}

// ---- Vec<f32> bridge -----------------------------------------

impl Type<Spg> for Vec<f32> {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Vector)
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Vector)
    }
}

impl<'q> Encode<'q, Spg> for Vec<f32> {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Vector(self.clone()),
            type_info: Some(<Vec<f32> as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for Vec<f32> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Vector(v) => Ok(v.clone()),
            EngineValue::Sq8Vector(q) => Ok(spg_storage::quantize::dequantize(q)),
            EngineValue::HalfVector(h) => Ok(h.to_f32_vec()),
            other => Err(format!("cannot decode {other:?} as Vec<f32> / VECTOR").into()),
        }
    }
}
