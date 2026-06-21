//! v7.16.0 — `Type` / `Encode` / `Decode` for the chrono date
//! / time types. SPG stores TIMESTAMP and TIMESTAMPTZ as `i64`
//! microseconds since the Unix epoch (always UTC for storage);
//! DATE as `i32` days. The Encode side renders to PG-canonical
//! text via the engine's `Value::Bytes`/text-coerce path so
//! the conversion matches v7.15.0's TIMESTAMPTZ literal parser.

#![cfg(feature = "chrono")]

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::ValueOwned as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

// ---- DateTime<Utc> (TIMESTAMPTZ) ----

impl Type<Spg> for DateTime<Utc> {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Timestamptz)
    }
    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Timestamptz | Kind::Timestamp)
    }
}

impl<'q> Encode<'q, Spg> for DateTime<Utc> {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        // Convert to PG-canonical `YYYY-MM-DD HH:MM:SS.fff+00`
        // text. The engine's v7.15.0 TIMESTAMPTZ parser accepts
        // this shape and stores as i64 µs UTC.
        let s = self.format("%Y-%m-%d %H:%M:%S%.6f+00").to_string();
        buf.push(SpgArgumentValue {
            value: EngineValue::text(s),
            type_info: Some(<DateTime<Utc> as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for DateTime<Utc> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Timestamp(micros) => {
                let secs = micros.div_euclid(1_000_000);
                let nanos = (micros.rem_euclid(1_000_000) as u32) * 1_000;
                let dt = DateTime::<Utc>::from_timestamp(secs, nanos)
                    .ok_or_else(|| format!("TIMESTAMPTZ value {micros} µs out of chrono range"))?;
                Ok(dt)
            }
            other => Err(format!("cannot decode {other:?} as chrono::DateTime<Utc>").into()),
        }
    }
}

// ---- NaiveDateTime (TIMESTAMP without TZ) ----

impl Type<Spg> for NaiveDateTime {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Timestamp)
    }
    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Timestamp | Kind::Timestamptz)
    }
}

impl<'q> Encode<'q, Spg> for NaiveDateTime {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        let s = self.format("%Y-%m-%d %H:%M:%S%.6f").to_string();
        buf.push(SpgArgumentValue {
            value: EngineValue::text(s),
            type_info: Some(<NaiveDateTime as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for NaiveDateTime {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Timestamp(micros) => {
                let secs = micros.div_euclid(1_000_000);
                let nanos = (micros.rem_euclid(1_000_000) as u32) * 1_000;
                let dt = DateTime::<Utc>::from_timestamp(secs, nanos)
                    .ok_or_else(|| format!("TIMESTAMP value {micros} µs out of chrono range"))?;
                Ok(dt.naive_utc())
            }
            other => Err(format!("cannot decode {other:?} as chrono::NaiveDateTime").into()),
        }
    }
}

// ---- NaiveDate (DATE) ----

impl Type<Spg> for NaiveDate {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Date)
    }
    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Date)
    }
}

impl<'q> Encode<'q, Spg> for NaiveDate {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        let s = format!("{:04}-{:02}-{:02}", self.year(), self.month(), self.day());
        buf.push(SpgArgumentValue {
            value: EngineValue::text(s),
            type_info: Some(<NaiveDate as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for NaiveDate {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Date(d) => {
                // SPG stores DATE as i32 days since Unix epoch.
                let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
                    .ok_or("1970-01-01 should be a valid date")?;
                epoch
                    .checked_add_signed(chrono::Duration::days(i64::from(*d)))
                    .ok_or_else(|| format!("DATE value {d} out of chrono range").into())
            }
            other => Err(format!("cannot decode {other:?} as chrono::NaiveDate").into()),
        }
    }
}

// Hush unused-imports — Timelike is reached only via the
// format!() chain on NaiveDateTime.
#[allow(dead_code)]
fn _timelike_marker(t: NaiveDateTime) -> u32 {
    t.hour()
}
