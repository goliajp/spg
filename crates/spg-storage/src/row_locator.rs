// `RowLocator` is the v5.1 PB-index value type; it crosses
// usize ↔ u32/u64 boundaries on serialisation. The casts are
// bounded by `RowLocator::MAX_HOT_INDEX` / `MAX_SEGMENT_ID` and
// surface as `RowLocatorError` rather than panicking.
#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

//! v5.1 — two-tier row pointer. The PB secondary index used to
//! map `IndexKey → Vec<usize>`, where each `usize` was a row
//! position in `Table::rows: PersistentVec<Row>` (the hot tier).
//! v5.1 widens that to `Vec<RowLocator>` so a single key can
//! point to a mix of rows in the in-memory hot tier and rows
//! that have been frozen to immutable cold-tier segment files
//! (`spg-storage::segment`).
//!
//! ## Why this is its own type, not just an `enum`
//!
//! The `RowLocator` carries two pieces of structural truth that
//! the v5 design pins:
//!
//! 1. **`Hot(usize)` is the v4 shape preserved.** Any existing
//!    PB index entry materialises as `Hot(row_index)` after the
//!    v8 → v9 catalog upgrade; readers that don't yet know about
//!    cold tiers can drop the `Cold` arm via `as_hot()` and
//!    behave exactly like v4.
//!
//! 2. **`Cold { segment_id, page_offset }` is self-contained.**
//!    The 32-bit `segment_id` indexes `Catalog::cold_segments`;
//!    `page_offset` is the byte offset of the page (already
//!    page-aligned, i.e. a multiple of `SEGMENT_PAGE_BYTES`)
//!    inside the segment file. The locator does **not** carry
//!    the row's within-page offset — that's recoverable via the
//!    segment's own binary search on the page given the lookup
//!    key, so the locator stays compact (8 bytes payload).
//!
//! ## Serialisation
//!
//! On-disk wire format used by the v5.1 catalog (file format v9):
//!
//! ```text
//! Hot(idx):    [u8 0x00][u64 LE idx]
//! Cold{s,p}:   [u8 0x01][u32 LE segment_id][u32 LE page_offset]
//! ```
//!
//! The tag byte is what lets a v9 reader disambiguate the
//! variants; v8 catalogs (which only ever wrote raw `u64` row
//! indices) are upgraded by the catalog-level decoder, not by
//! `read_le` here — keeping the locator's wire format clean.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

const TAG_HOT: u8 = 0x00;
const TAG_COLD: u8 = 0x01;

/// Errors surfaced by `RowLocator::read_le` when the byte slice
/// doesn't match either tagged variant layout.
#[derive(Debug, PartialEq, Eq)]
pub enum RowLocatorError {
    /// Slice was shorter than the minimum tagged-variant length.
    TooShort { got: usize, need: usize },
    /// Tag byte wasn't `TAG_HOT` (0x00) or `TAG_COLD` (0x01).
    BadTag { got: u8 },
    /// Caller used `read_le` with a `Cold` payload but the slice
    /// stopped before the 8 bytes of `(segment_id, page_offset)`.
    TruncatedCold { got: usize, need: usize },
    /// Catalog format v8 fallback: caller asked `read_le_legacy_u64`
    /// to wrap a row index that doesn't fit in `usize` on this
    /// target (a 32-bit target reading a v8 catalog with > 4 G
    /// rows per table). Surface explicitly rather than wrapping.
    LegacyIndexOverflow(String),
}

impl fmt::Display for RowLocatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { got, need } => {
                write!(f, "row_locator: too short, got {got} bytes, need {need}")
            }
            Self::BadTag { got } => write!(
                f,
                "row_locator: bad tag 0x{got:02x}, expected 0x00 (Hot) or 0x01 (Cold)"
            ),
            Self::TruncatedCold { got, need } => write!(
                f,
                "row_locator: cold variant truncated, got {got} bytes, need {need}"
            ),
            Self::LegacyIndexOverflow(s) => write!(f, "row_locator: legacy v8 index overflow: {s}"),
        }
    }
}

/// Two-tier row pointer; PB index value type after v5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RowLocator {
    /// Row lives in `Table::rows` (in-memory hot tier). The
    /// `usize` is the row's index into that `PersistentVec<Row>`
    /// — same semantics as the pre-v5 `usize` value the PB used
    /// to store.
    Hot(usize),
    /// Row lives in a cold-tier segment file. `segment_id`
    /// indexes `Catalog::cold_segments`; `page_offset` is the
    /// byte offset of the containing page inside the segment
    /// file (multiple of `SEGMENT_PAGE_BYTES`). The within-page
    /// row position is recovered by binary-searching the page
    /// at lookup time using the user's PK as the search key.
    Cold { segment_id: u32, page_offset: u32 },
}

impl RowLocator {
    /// True if this locator points into the hot tier.
    #[must_use]
    pub const fn is_hot(&self) -> bool {
        matches!(self, Self::Hot(_))
    }

    /// True if this locator points into a cold segment.
    #[must_use]
    pub const fn is_cold(&self) -> bool {
        matches!(self, Self::Cold { .. })
    }

    /// Extract the hot-tier row index, or `None` if cold.
    #[must_use]
    pub const fn as_hot(&self) -> Option<usize> {
        match self {
            Self::Hot(i) => Some(*i),
            Self::Cold { .. } => None,
        }
    }

    /// Extract the cold `(segment_id, page_offset)` pair, or
    /// `None` if hot.
    #[must_use]
    pub const fn as_cold(&self) -> Option<(u32, u32)> {
        match self {
            Self::Cold {
                segment_id,
                page_offset,
            } => Some((*segment_id, *page_offset)),
            Self::Hot(_) => None,
        }
    }

    /// Wire byte count if serialised by `write_le`. Constant per
    /// variant: 9 for Hot (tag + u64), 9 for Cold (tag + u32 +
    /// u32). Used by the catalog encoder to pre-size buffers.
    #[must_use]
    pub const fn encoded_len(&self) -> usize {
        // Both variants encode to exactly 9 bytes. This is
        // intentional: callers can `Vec::with_capacity(N × 9)`
        // without branching, and the v9 catalog decoder reads
        // exactly 9 bytes per locator without a length prefix.
        9
    }

    /// Append the wire representation to `out`. See the module
    /// doc for the layout.
    pub fn write_le(&self, out: &mut Vec<u8>) {
        match self {
            Self::Hot(i) => {
                out.push(TAG_HOT);
                out.extend_from_slice(&(*i as u64).to_le_bytes());
            }
            Self::Cold {
                segment_id,
                page_offset,
            } => {
                out.push(TAG_COLD);
                out.extend_from_slice(&segment_id.to_le_bytes());
                out.extend_from_slice(&page_offset.to_le_bytes());
            }
        }
    }

    /// Parse one locator from the start of `input`. Returns the
    /// locator + the number of bytes consumed (always 9 in v1).
    pub fn read_le(input: &[u8]) -> Result<(Self, usize), RowLocatorError> {
        if input.is_empty() {
            return Err(RowLocatorError::TooShort { got: 0, need: 1 });
        }
        let tag = input[0];
        match tag {
            TAG_HOT => {
                if input.len() < 9 {
                    return Err(RowLocatorError::TooShort {
                        got: input.len(),
                        need: 9,
                    });
                }
                let idx = u64::from_le_bytes([
                    input[1], input[2], input[3], input[4], input[5], input[6], input[7], input[8],
                ]);
                // u64 → usize: on 64-bit targets identity; on
                // 32-bit targets fail rather than truncate.
                let idx_usize = usize::try_from(idx).map_err(|_| {
                    RowLocatorError::LegacyIndexOverflow(format!(
                        "Hot row index {idx} exceeds usize on this target"
                    ))
                })?;
                Ok((Self::Hot(idx_usize), 9))
            }
            TAG_COLD => {
                if input.len() < 9 {
                    return Err(RowLocatorError::TruncatedCold {
                        got: input.len(),
                        need: 9,
                    });
                }
                let segment_id = u32::from_le_bytes([input[1], input[2], input[3], input[4]]);
                let page_offset = u32::from_le_bytes([input[5], input[6], input[7], input[8]]);
                Ok((
                    Self::Cold {
                        segment_id,
                        page_offset,
                    },
                    9,
                ))
            }
            other => Err(RowLocatorError::BadTag { got: other }),
        }
    }

    /// Wrap a raw `u64` row index from a v8 catalog stream as a
    /// `RowLocator::Hot(_)`. Catalog format v8 stored bare u64
    /// row indices without a tag byte; the v9 reader uses this
    /// to upgrade-in-place rather than rejecting v8 input. Fails
    /// only if the index doesn't fit in `usize` on this target.
    pub fn from_legacy_v8_u64(idx: u64) -> Result<Self, RowLocatorError> {
        let idx_usize = usize::try_from(idx).map_err(|_| {
            RowLocatorError::LegacyIndexOverflow(format!(
                "Hot row index {idx} exceeds usize on this target"
            ))
        })?;
        Ok(Self::Hot(idx_usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn hot_constructs_and_inspects() {
        let l = RowLocator::Hot(42);
        assert!(l.is_hot());
        assert!(!l.is_cold());
        assert_eq!(l.as_hot(), Some(42));
        assert_eq!(l.as_cold(), None);
    }

    #[test]
    fn cold_constructs_and_inspects() {
        let l = RowLocator::Cold {
            segment_id: 7,
            page_offset: 4096 * 9,
        };
        assert!(l.is_cold());
        assert!(!l.is_hot());
        assert_eq!(l.as_hot(), None);
        assert_eq!(l.as_cold(), Some((7, 36_864)));
    }

    #[test]
    fn encoded_len_is_constant() {
        assert_eq!(RowLocator::Hot(0).encoded_len(), 9);
        assert_eq!(RowLocator::Hot(usize::MAX).encoded_len(), 9);
        assert_eq!(
            RowLocator::Cold {
                segment_id: u32::MAX,
                page_offset: u32::MAX,
            }
            .encoded_len(),
            9
        );
    }

    #[test]
    fn roundtrip_hot_via_write_le_read_le() {
        for &idx in &[0_usize, 1, 42, 1_000_000, usize::MAX] {
            let l = RowLocator::Hot(idx);
            let mut buf = Vec::new();
            l.write_le(&mut buf);
            assert_eq!(buf.len(), 9);
            let (parsed, consumed) = RowLocator::read_le(&buf).expect("hot roundtrip parses");
            assert_eq!(parsed, l);
            assert_eq!(consumed, 9);
        }
    }

    #[test]
    fn roundtrip_cold_via_write_le_read_le() {
        for &(s, p) in &[
            (0_u32, 0_u32),
            (1, 4096),
            (42, 4096 * 7),
            (u32::MAX, u32::MAX),
        ] {
            let l = RowLocator::Cold {
                segment_id: s,
                page_offset: p,
            };
            let mut buf = Vec::new();
            l.write_le(&mut buf);
            assert_eq!(buf.len(), 9);
            let (parsed, consumed) = RowLocator::read_le(&buf).expect("cold roundtrip parses");
            assert_eq!(parsed, l);
            assert_eq!(consumed, 9);
        }
    }

    #[test]
    fn mixed_concat_decodes_in_sequence() {
        let entries = [
            RowLocator::Hot(7),
            RowLocator::Cold {
                segment_id: 2,
                page_offset: 4096,
            },
            RowLocator::Hot(99),
        ];
        let mut buf = Vec::new();
        for e in &entries {
            e.write_le(&mut buf);
        }
        assert_eq!(buf.len(), 27);
        let mut offset = 0;
        let mut decoded = Vec::new();
        while offset < buf.len() {
            let (l, n) = RowLocator::read_le(&buf[offset..]).expect("decode succeeds");
            decoded.push(l);
            offset += n;
        }
        assert_eq!(offset, buf.len());
        assert_eq!(decoded.as_slice(), entries.as_slice());
    }

    #[test]
    fn read_le_rejects_empty_input() {
        match RowLocator::read_le(&[]) {
            Err(RowLocatorError::TooShort { got: 0, need: 1 }) => {}
            other => panic!("expected TooShort, got {other:?}"),
        }
    }

    #[test]
    fn read_le_rejects_bad_tag() {
        // Tag 0xff isn't Hot (0x00) or Cold (0x01).
        let mut buf = vec![0xff_u8];
        buf.extend_from_slice(&0_u64.to_le_bytes());
        match RowLocator::read_le(&buf) {
            Err(RowLocatorError::BadTag { got: 0xff }) => {}
            other => panic!("expected BadTag, got {other:?}"),
        }
    }

    #[test]
    fn read_le_rejects_truncated_hot() {
        // Valid Hot tag but only 4 bytes of payload (need 8).
        let buf = [TAG_HOT, 0x01, 0x02, 0x03, 0x04];
        match RowLocator::read_le(&buf) {
            Err(RowLocatorError::TooShort { got: 5, need: 9 }) => {}
            other => panic!("expected TooShort, got {other:?}"),
        }
    }

    #[test]
    fn read_le_rejects_truncated_cold() {
        let buf = [TAG_COLD, 0x01, 0x02, 0x03];
        match RowLocator::read_le(&buf) {
            Err(RowLocatorError::TruncatedCold { got: 4, need: 9 }) => {}
            other => panic!("expected TruncatedCold, got {other:?}"),
        }
    }

    #[test]
    fn from_legacy_v8_u64_wraps_as_hot() {
        for &idx in &[0_u64, 1, 1_000_000, u64::from(u32::MAX)] {
            let l = RowLocator::from_legacy_v8_u64(idx).expect("fits usize on 64-bit");
            assert_eq!(l.as_hot(), Some(idx as usize));
        }
    }

    /// Default enum repr is 16 bytes (8-byte discriminant + 8-byte
    /// max payload). The v5.1 design accepts this and revisits
    /// only if PB perf gate measurements show regression. Pin the
    /// size here so a future repr change is caught explicitly.
    #[test]
    fn size_is_16_bytes() {
        assert_eq!(core::mem::size_of::<RowLocator>(), 16);
    }
}
