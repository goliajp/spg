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

/// Every collation MySQL 9.7.2 serves, read from its own
/// `information_schema.collations` — 286 names over 41 character sets,
/// sorted for `binary_search`.
///
/// v7.39.2 — the check this replaces was a SUFFIX: any name ending in
/// `_ci`, `_cs` or `_bin` counted as known. `SELECT 'a' COLLATE
/// nosuch_ci` was therefore accepted and then silently ignored — the
/// comparison did whatever the session would have done anyway, and a
/// client that named a collation got no signal that it had not been
/// used. MySQL 9.7.2 answers `ERROR 1273 (HY000): Unknown collation:
/// 'nosuch_ci'`.
pub const MYSQL_COLLATIONS: &[&str] = &[
    "armscii8_bin",
    "armscii8_general_ci",
    "ascii_bin",
    "ascii_general_ci",
    "big5_bin",
    "big5_chinese_ci",
    "binary",
    "cp1250_bin",
    "cp1250_croatian_ci",
    "cp1250_czech_cs",
    "cp1250_general_ci",
    "cp1250_polish_ci",
    "cp1251_bin",
    "cp1251_bulgarian_ci",
    "cp1251_general_ci",
    "cp1251_general_cs",
    "cp1251_ukrainian_ci",
    "cp1256_bin",
    "cp1256_general_ci",
    "cp1257_bin",
    "cp1257_general_ci",
    "cp1257_lithuanian_ci",
    "cp850_bin",
    "cp850_general_ci",
    "cp852_bin",
    "cp852_general_ci",
    "cp866_bin",
    "cp866_general_ci",
    "cp932_bin",
    "cp932_japanese_ci",
    "dec8_bin",
    "dec8_swedish_ci",
    "eucjpms_bin",
    "eucjpms_japanese_ci",
    "euckr_bin",
    "euckr_korean_ci",
    "gb18030_bin",
    "gb18030_chinese_ci",
    "gb18030_unicode_520_ci",
    "gb2312_bin",
    "gb2312_chinese_ci",
    "gbk_bin",
    "gbk_chinese_ci",
    "geostd8_bin",
    "geostd8_general_ci",
    "greek_bin",
    "greek_general_ci",
    "hebrew_bin",
    "hebrew_general_ci",
    "hp8_bin",
    "hp8_english_ci",
    "keybcs2_bin",
    "keybcs2_general_ci",
    "koi8r_bin",
    "koi8r_general_ci",
    "koi8u_bin",
    "koi8u_general_ci",
    "latin1_bin",
    "latin1_danish_ci",
    "latin1_general_ci",
    "latin1_general_cs",
    "latin1_german1_ci",
    "latin1_german2_ci",
    "latin1_spanish_ci",
    "latin1_swedish_ci",
    "latin2_bin",
    "latin2_croatian_ci",
    "latin2_czech_cs",
    "latin2_general_ci",
    "latin2_hungarian_ci",
    "latin5_bin",
    "latin5_turkish_ci",
    "latin7_bin",
    "latin7_estonian_cs",
    "latin7_general_ci",
    "latin7_general_cs",
    "macce_bin",
    "macce_general_ci",
    "macroman_bin",
    "macroman_general_ci",
    "sjis_bin",
    "sjis_japanese_ci",
    "swe7_bin",
    "swe7_swedish_ci",
    "tis620_bin",
    "tis620_thai_ci",
    "ucs2_bin",
    "ucs2_croatian_ci",
    "ucs2_czech_ci",
    "ucs2_danish_ci",
    "ucs2_esperanto_ci",
    "ucs2_estonian_ci",
    "ucs2_general_ci",
    "ucs2_general_mysql500_ci",
    "ucs2_german2_ci",
    "ucs2_hungarian_ci",
    "ucs2_icelandic_ci",
    "ucs2_latvian_ci",
    "ucs2_lithuanian_ci",
    "ucs2_persian_ci",
    "ucs2_polish_ci",
    "ucs2_roman_ci",
    "ucs2_romanian_ci",
    "ucs2_sinhala_ci",
    "ucs2_slovak_ci",
    "ucs2_slovenian_ci",
    "ucs2_spanish2_ci",
    "ucs2_spanish_ci",
    "ucs2_swedish_ci",
    "ucs2_turkish_ci",
    "ucs2_unicode_520_ci",
    "ucs2_unicode_ci",
    "ucs2_vietnamese_ci",
    "ujis_bin",
    "ujis_japanese_ci",
    "utf16_bin",
    "utf16_croatian_ci",
    "utf16_czech_ci",
    "utf16_danish_ci",
    "utf16_esperanto_ci",
    "utf16_estonian_ci",
    "utf16_general_ci",
    "utf16_german2_ci",
    "utf16_hungarian_ci",
    "utf16_icelandic_ci",
    "utf16_latvian_ci",
    "utf16_lithuanian_ci",
    "utf16_persian_ci",
    "utf16_polish_ci",
    "utf16_roman_ci",
    "utf16_romanian_ci",
    "utf16_sinhala_ci",
    "utf16_slovak_ci",
    "utf16_slovenian_ci",
    "utf16_spanish2_ci",
    "utf16_spanish_ci",
    "utf16_swedish_ci",
    "utf16_turkish_ci",
    "utf16_unicode_520_ci",
    "utf16_unicode_ci",
    "utf16_vietnamese_ci",
    "utf16le_bin",
    "utf16le_general_ci",
    "utf32_bin",
    "utf32_croatian_ci",
    "utf32_czech_ci",
    "utf32_danish_ci",
    "utf32_esperanto_ci",
    "utf32_estonian_ci",
    "utf32_general_ci",
    "utf32_german2_ci",
    "utf32_hungarian_ci",
    "utf32_icelandic_ci",
    "utf32_latvian_ci",
    "utf32_lithuanian_ci",
    "utf32_persian_ci",
    "utf32_polish_ci",
    "utf32_roman_ci",
    "utf32_romanian_ci",
    "utf32_sinhala_ci",
    "utf32_slovak_ci",
    "utf32_slovenian_ci",
    "utf32_spanish2_ci",
    "utf32_spanish_ci",
    "utf32_swedish_ci",
    "utf32_turkish_ci",
    "utf32_unicode_520_ci",
    "utf32_unicode_ci",
    "utf32_vietnamese_ci",
    "utf8mb3_bin",
    "utf8mb3_croatian_ci",
    "utf8mb3_czech_ci",
    "utf8mb3_danish_ci",
    "utf8mb3_esperanto_ci",
    "utf8mb3_estonian_ci",
    "utf8mb3_general_ci",
    "utf8mb3_general_mysql500_ci",
    "utf8mb3_german2_ci",
    "utf8mb3_hungarian_ci",
    "utf8mb3_icelandic_ci",
    "utf8mb3_latvian_ci",
    "utf8mb3_lithuanian_ci",
    "utf8mb3_persian_ci",
    "utf8mb3_polish_ci",
    "utf8mb3_roman_ci",
    "utf8mb3_romanian_ci",
    "utf8mb3_sinhala_ci",
    "utf8mb3_slovak_ci",
    "utf8mb3_slovenian_ci",
    "utf8mb3_spanish2_ci",
    "utf8mb3_spanish_ci",
    "utf8mb3_swedish_ci",
    "utf8mb3_tolower_ci",
    "utf8mb3_turkish_ci",
    "utf8mb3_unicode_520_ci",
    "utf8mb3_unicode_ci",
    "utf8mb3_vietnamese_ci",
    "utf8mb4_0900_ai_ci",
    "utf8mb4_0900_as_ci",
    "utf8mb4_0900_as_cs",
    "utf8mb4_0900_bin",
    "utf8mb4_bg_0900_ai_ci",
    "utf8mb4_bg_0900_as_cs",
    "utf8mb4_bin",
    "utf8mb4_bs_0900_ai_ci",
    "utf8mb4_bs_0900_as_cs",
    "utf8mb4_croatian_ci",
    "utf8mb4_cs_0900_ai_ci",
    "utf8mb4_cs_0900_as_cs",
    "utf8mb4_czech_ci",
    "utf8mb4_da_0900_ai_ci",
    "utf8mb4_da_0900_as_cs",
    "utf8mb4_danish_ci",
    "utf8mb4_de_pb_0900_ai_ci",
    "utf8mb4_de_pb_0900_as_cs",
    "utf8mb4_eo_0900_ai_ci",
    "utf8mb4_eo_0900_as_cs",
    "utf8mb4_es_0900_ai_ci",
    "utf8mb4_es_0900_as_cs",
    "utf8mb4_es_trad_0900_ai_ci",
    "utf8mb4_es_trad_0900_as_cs",
    "utf8mb4_esperanto_ci",
    "utf8mb4_estonian_ci",
    "utf8mb4_et_0900_ai_ci",
    "utf8mb4_et_0900_as_cs",
    "utf8mb4_general_ci",
    "utf8mb4_german2_ci",
    "utf8mb4_gl_0900_ai_ci",
    "utf8mb4_gl_0900_as_cs",
    "utf8mb4_hr_0900_ai_ci",
    "utf8mb4_hr_0900_as_cs",
    "utf8mb4_hu_0900_ai_ci",
    "utf8mb4_hu_0900_as_cs",
    "utf8mb4_hungarian_ci",
    "utf8mb4_icelandic_ci",
    "utf8mb4_is_0900_ai_ci",
    "utf8mb4_is_0900_as_cs",
    "utf8mb4_ja_0900_as_cs",
    "utf8mb4_ja_0900_as_cs_ks",
    "utf8mb4_la_0900_ai_ci",
    "utf8mb4_la_0900_as_cs",
    "utf8mb4_latvian_ci",
    "utf8mb4_lithuanian_ci",
    "utf8mb4_lt_0900_ai_ci",
    "utf8mb4_lt_0900_as_cs",
    "utf8mb4_lv_0900_ai_ci",
    "utf8mb4_lv_0900_as_cs",
    "utf8mb4_mn_cyrl_0900_ai_ci",
    "utf8mb4_mn_cyrl_0900_as_cs",
    "utf8mb4_nb_0900_ai_ci",
    "utf8mb4_nb_0900_as_cs",
    "utf8mb4_nn_0900_ai_ci",
    "utf8mb4_nn_0900_as_cs",
    "utf8mb4_persian_ci",
    "utf8mb4_pl_0900_ai_ci",
    "utf8mb4_pl_0900_as_cs",
    "utf8mb4_polish_ci",
    "utf8mb4_ro_0900_ai_ci",
    "utf8mb4_ro_0900_as_cs",
    "utf8mb4_roman_ci",
    "utf8mb4_romanian_ci",
    "utf8mb4_ru_0900_ai_ci",
    "utf8mb4_ru_0900_as_cs",
    "utf8mb4_sinhala_ci",
    "utf8mb4_sk_0900_ai_ci",
    "utf8mb4_sk_0900_as_cs",
    "utf8mb4_sl_0900_ai_ci",
    "utf8mb4_sl_0900_as_cs",
    "utf8mb4_slovak_ci",
    "utf8mb4_slovenian_ci",
    "utf8mb4_spanish2_ci",
    "utf8mb4_spanish_ci",
    "utf8mb4_sr_latn_0900_ai_ci",
    "utf8mb4_sr_latn_0900_as_cs",
    "utf8mb4_sv_0900_ai_ci",
    "utf8mb4_sv_0900_as_cs",
    "utf8mb4_swedish_ci",
    "utf8mb4_tr_0900_ai_ci",
    "utf8mb4_tr_0900_as_cs",
    "utf8mb4_turkish_ci",
    "utf8mb4_unicode_520_ci",
    "utf8mb4_unicode_ci",
    "utf8mb4_vi_0900_ai_ci",
    "utf8mb4_vi_0900_as_cs",
    "utf8mb4_vietnamese_ci",
    "utf8mb4_zh_0900_as_cs",
];

/// Is this one of MySQL's collation names?
#[must_use]
pub fn is_mysql_collation(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    MYSQL_COLLATIONS.binary_search(&lower.as_str()).is_ok()
}

#[cfg(test)]
mod mysql_collation_tests {
    use super::{MYSQL_COLLATIONS, is_mysql_collation};

    /// `binary_search` is only an answer if the list is in byte order,
    /// and MySQL's own `ORDER BY` is not byte order — the first version
    /// of this table was written in the order the server returned and
    /// would have missed names in the middle of it.
    #[test]
    fn the_table_is_sorted_and_lowercase() {
        let mut sorted = MYSQL_COLLATIONS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted.as_slice(), MYSQL_COLLATIONS, "not in byte order");
        assert!(
            MYSQL_COLLATIONS
                .iter()
                .all(|c| c.to_ascii_lowercase() == **c),
            "a name is not lowercase, so the lookup would miss it"
        );
        // Every one of them must be findable, which a wrong order breaks
        // silently for exactly the names a suffix rule already accepted.
        assert!(MYSQL_COLLATIONS.iter().all(|c| is_mysql_collation(c)));
    }

    #[test]
    fn it_answers_the_names_the_suffix_rule_could_not_tell_apart() {
        assert!(is_mysql_collation("utf8mb4_general_ci"));
        assert!(is_mysql_collation("utf8mb4_0900_ai_ci"));
        assert!(is_mysql_collation("UTF8MB4_BIN"));
        assert!(is_mysql_collation("binary"));
        assert!(is_mysql_collation("latin1_swedish_ci"));
        // All three suffixes the old rule waved through.
        assert!(!is_mysql_collation("nosuch_ci"));
        assert!(!is_mysql_collation("nosuch_cs"));
        assert!(!is_mysql_collation("nosuch_bin"));
        // And a real charset with a family MySQL does not have.
        assert!(!is_mysql_collation("utf8mb4_madeup_ci"));
    }
}
