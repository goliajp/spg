//! MySQL charset names and the collation each one defaults to.
//!
//! v7.39.2 — moved here from `spg-engine`'s `collate` because the
//! PARSER needs it: `_utf8mb4'x'` is a charset introducer only when the
//! word after the underscore names a charset, and deciding that is
//! parsing. The engine reads the same table through this module rather
//! than keeping a second copy — two lists of one fact is how the
//! surfaces in this release came to disagree in the first place.

/// Do trailing spaces count in a comparison under this collation?
///
/// v7.38.18 — the pad attribute is a property of the collation NAME,
/// and SPG read a name for whether to FOLD and never for whether to
/// PAD. `utf8mb4_bin` and `utf8mb4_general_ci` are PAD SPACE in BOTH
/// engines, which is the part that is easy to get backwards: byte-wise
/// does not mean padding counts.
///
/// The rule is measured, not inferred. Over every collation both
/// oracles list — 286 names on MySQL 9.7.2 and 552 on MariaDB 12.3.2,
/// 838 in all — `NO PAD` holds exactly when the name is `binary`, or
/// contains `_0900_`, or contains `nopad`. **Zero counter-examples on
/// either engine.**
///
/// `None` — nothing declared — is the session default, and SPG
/// advertises `8.0.0-spg-v…` on the MySQL wire, so that is MySQL 8.0's
/// `utf8mb4_0900_ai_ci`: NO PAD.
/// v7.39 — the collation a MySQL session compares under when the
/// session has not said otherwise.
///
/// One constant, because the last time a name SPG reports and a rule SPG
/// applies lived in two places they disagreed for months
/// (`server_version`, v7.38.25) -- and they disagreed here too:
/// `@@collation_connection` answered `utf8mb4_general_ci` (PAD SPACE)
/// while `e2e_mysql_pad_space_round375` pinned the NO PAD behaviour of
/// `utf8mb4_0900_ai_ci`, re-measured on MySQL 9.7.2 in v7.38.17. Both
/// were defensible on their own and could not both be right.
///
/// `utf8mb4_0900_ai_ci` is the value: it is what SPG's advertised 8.0
/// lineage implies, what `show.rs` already reported for
/// `collation_server`, what MySQL 9.7.2 answers for a bare
/// `SET NAMES utf8mb4` (measured -- and `'a' = 'a '` is 0 there), and
/// what those pins were re-measured against. `pads_space` reads
/// `_0900_` and says NO PAD, so the reported name and the applied rule
/// now agree by construction.
/// v7.39 — `SET NAMES <charset>` with no `COLLATE` takes the charset's
/// DEFAULT collation, and that decides whether the session pads.
/// Measured, not inferred: read from MySQL 9.7.2's
/// `information_schema.CHARACTER_SETS`, all 41 rows of it, so
/// the two that matter most are visible beside the rest --
/// `utf8mb4` -> `utf8mb4_0900_ai_ci` (NO PAD) and `latin1` ->
/// `latin1_swedish_ci` (PAD SPACE). Confirmed on the wire: a bare
/// `SET NAMES utf8mb4` answers `'a' = 'a '` with 0 there and a bare
/// `SET NAMES latin1` answers 1.
pub const MYSQL_CHARSET_DEFAULT_COLLATION: &[(&str, &str)] = &[
    ("armscii8", "armscii8_general_ci"),
    ("ascii", "ascii_general_ci"),
    ("big5", "big5_chinese_ci"),
    ("binary", "binary"),
    ("cp1250", "cp1250_general_ci"),
    ("cp1251", "cp1251_general_ci"),
    ("cp1256", "cp1256_general_ci"),
    ("cp1257", "cp1257_general_ci"),
    ("cp850", "cp850_general_ci"),
    ("cp852", "cp852_general_ci"),
    ("cp866", "cp866_general_ci"),
    ("cp932", "cp932_japanese_ci"),
    ("dec8", "dec8_swedish_ci"),
    ("eucjpms", "eucjpms_japanese_ci"),
    ("euckr", "euckr_korean_ci"),
    ("gb18030", "gb18030_chinese_ci"),
    ("gb2312", "gb2312_chinese_ci"),
    ("gbk", "gbk_chinese_ci"),
    ("geostd8", "geostd8_general_ci"),
    ("greek", "greek_general_ci"),
    ("hebrew", "hebrew_general_ci"),
    ("hp8", "hp8_english_ci"),
    ("keybcs2", "keybcs2_general_ci"),
    ("koi8r", "koi8r_general_ci"),
    ("koi8u", "koi8u_general_ci"),
    ("latin1", "latin1_swedish_ci"),
    ("latin2", "latin2_general_ci"),
    ("latin5", "latin5_turkish_ci"),
    ("latin7", "latin7_general_ci"),
    ("macce", "macce_general_ci"),
    ("macroman", "macroman_general_ci"),
    ("sjis", "sjis_japanese_ci"),
    ("swe7", "swe7_swedish_ci"),
    ("tis620", "tis620_thai_ci"),
    ("ucs2", "ucs2_general_ci"),
    ("ujis", "ujis_japanese_ci"),
    ("utf16", "utf16_general_ci"),
    ("utf16le", "utf16le_general_ci"),
    ("utf32", "utf32_general_ci"),
    ("utf8mb3", "utf8mb3_general_ci"),
    ("utf8mb4", "utf8mb4_0900_ai_ci"),
];

/// The collation `SET NAMES <charset>` selects, or `None` for a charset
/// this build does not know. An unknown name is not silently mapped to
/// a default: that is how a session ends up padding differently from
/// what it was told.
#[must_use]
pub fn charset_default_collation(charset: &str) -> Option<&'static str> {
    let lower = charset.trim().to_ascii_lowercase();
    MYSQL_CHARSET_DEFAULT_COLLATION
        .iter()
        .find(|(c, _)| *c == lower)
        .map(|(_, coll)| *coll)
}
