//! Locale collation — the comparison primitive, on its own.
//!
//! v7.39 (round 678) — F36 step three, and deliberately only the piece that
//! can stand alone. Nothing in the engine calls this yet: `value_cmp` takes
//! two values and no column, and the collation is a property of the COLUMN,
//! so making `ORDER BY name` honour a declaration means carrying it to 47
//! comparison sites. That threading is its own round. This is the part that
//! can be built and tested without it.
//!
//! Route chosen in `docs/COLLATION_RFC.md` by measurement: libc `strcoll`
//! was rejected because it orders by the locales installed on the host — an
//! `ORDER BY` whose answer depends on which machine the database runs on —
//! and because the three core crates are `#![no_std]`. ICU4X compiles under
//! no_std and reproduced every probe taken off PG18.

use alloc::string::String;
use core::cmp::Ordering;

/// Compare two strings under a named collation.
///
/// `None` means the name is not one this build can perform, and the caller
/// keeps whatever it would have done. That matters: F36 records
/// accepting-a-declaration-and-ignoring-it as the defect, and quietly
/// substituting a DIFFERENT collation would be a second way to do the same
/// thing.
///
/// `C` and `POSIX` are answered here rather than handed to ICU, because
/// they are byte order by definition and SPG performs them already.
pub(crate) fn compare(collation: &str, a: &str, b: &str) -> Option<Ordering> {
    let name = collation.trim();
    // v7.39 (round 680) — the encoding suffix rides along on these too:
    // PG18 publishes `C.utf8` beside `C`, and a survey of all 880 of its
    // collation names found it among only three this build could not
    // perform.
    let base = name.split(['.', '@']).next().unwrap_or(name);
    if base.eq_ignore_ascii_case("C") || base.eq_ignore_ascii_case("POSIX") {
        return Some(a.as_bytes().cmp(b.as_bytes()));
    }
    let locale = icu_locale_core::Locale::try_from_str(&normalise(name)).ok()?;
    let prefs = icu_collator::CollatorPreferences::from(&locale);
    let collator = icu_collator::Collator::try_new(prefs, pg_options()).ok()?;
    // v7.39 (round 690) — the deterministic tiebreak. PG18's `en_US.utf8`
    // has `collisdeterministic = t`, which means a collation tie is broken
    // by the bytes, and it is observable: `e` + U+0301 and U+00E9 are
    // canonically equivalent, so ICU calls them Equal, but PG18 orders the
    // decomposed form FIRST (0x65 < 0xC3) and reports `=` as false.
    // Without this, two such values would sort in whatever order the sort
    // happened to leave them in.
    Some(
        collator
            .compare(a, b)
            .then_with(|| a.as_bytes().cmp(b.as_bytes())),
    )
}

/// The options PG's collations behave under.
///
/// v7.39 (round 683) — `AlternateHandling::Shifted`, and it is not a
/// preference. ICU defaults to `NonIgnorable`, where punctuation carries a
/// primary weight; PG (via glibc/ICU as configured) uses shifted, where it
/// is ignorable at the primary level and only breaks ties. Measured on
/// PG18: `_under` sorts between `cherry` and `Zebra` — as `under` — and
/// with the ICU default it sorts before all of them.
///
/// Round 678's rules missed this because the probe used single characters.
/// With one character per value every primary weight is equal, so the
/// tiebreak IS the answer, and " < _ < 1 < a" got recorded as the rule when
/// it is only the degenerate case.
fn pg_options() -> icu_collator::options::CollatorOptions {
    // v7.39 (round 684) — `Shifted`, max_variable `Punctuation`. Chosen by
    // sweeping all five settings against PG, not by preference.
    //
    // Punctuation is variable-weighted in PG: ignorable at the primary
    // level, a tiebreak below it. `_id` sorts as `id`, `O'Brien` next to
    // `Obrien`, `de-luca` next to `deluca` — measured, all three.
    //
    // One case used to stay wrong and no setting fixed it: ordering values
    // that are ENTIRELY punctuation, PG gave ` , -, ., _` and shifted ICU
    // gave `_,  , ...`. Rounds 683/684 recorded that as F36's residual and
    // took the trade — NonIgnorable got that one shape right and every
    // shape with a letter in it wrong.
    //
    // v7.39 (round 690) — it closed as a side effect of the deterministic
    // tiebreak in `compare`, and the reason is worth keeping: with no
    // letters, ICU finds these EQUAL at every level, so what used to decide
    // was nothing at all. The bytes decide now, and PG's answer for this
    // shape IS byte order (` ` 0x20 < `-` 0x2D < `.` 0x2E < `_` 0x5F).
    // Re-measured against PG18, not assumed.
    let mut o = icu_collator::options::CollatorOptions::default();
    o.alternate_handling = Some(icu_collator::options::AlternateHandling::Shifted);
    o.max_variable = Some(icu_collator::options::MaxVariable::Punctuation);
    o
}

/// Whether this build can perform a collation by that name.
pub(crate) fn is_supported(collation: &str) -> bool {
    compare(collation, "a", "b").is_some()
}

/// PG spells locales the POSIX way (`en_US.utf8`, `de_DE@euro`); BCP-47
/// wants `en-US`. Strip the encoding and the modifier, swap the separator.
///
/// `default` is not a locale at all — it names whatever the database was
/// created with, which for SPG is C.
fn normalise(name: &str) -> String {
    // PG's own names for the root collation. `default` is whatever the
    // database was created with, which for SPG is C; `unicode` and
    // `pg_unicode_fast` are PG18's spellings of the UCA root, which is ICU's
    // `und`. The round-680 survey found these two the last of PG's 880 that
    // this build did not answer.
    if name.eq_ignore_ascii_case("default")
        || name.eq_ignore_ascii_case("unicode")
        || name.eq_ignore_ascii_case("pg_unicode_fast")
        || name.eq_ignore_ascii_case("ucs_basic")
    {
        return String::from("und");
    }
    let head = name.split(['.', '@']).next().unwrap_or(name);
    head.replace('_', "-")
}

#[cfg(test)]
#[path = "collate_survey.rs"]
mod survey;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn sorted<'a>(coll: &str, mut xs: Vec<&'a str>) -> Vec<&'a str> {
        xs.sort_by(|a, b| compare(coll, a, b).expect("supported"));
        xs
    }

    /// The seven rules, read off PG18 running `en_US.utf8` rather than out
    /// of anyone's source. Each one is a property that byte order gets
    /// wrong, so together they are what "locale collation" has to mean.
    #[test]
    fn en_us_reproduces_the_rules_measured_from_pg() {
        // Primary weight ignores case; the tertiary one puts lowercase first.
        assert_eq!(sorted("en_US.utf8", vec!["b", "A", "a", "B"]), ["a", "A", "b", "B"]);
        assert_eq!(compare("en_US.utf8", "a", "A"), Some(Ordering::Less));
        // Accents are SECONDARY: primary treats é as e, so a shorter string
        // wins before the accent is ever consulted.
        assert_eq!(
            sorted("en_US.utf8", vec!["f", "ê", "E", "é", "e"]),
            ["e", "E", "é", "ê", "f"]
        );
        assert_eq!(compare("en_US.utf8", "résumé", "resumes"), Some(Ordering::Less));
        // Digits before letters. Punctuation's place among them is a
        // quaternary question once nothing else separates the values — see
        // `all_punctuation_values_now_match_pg`.
        assert_eq!(sorted("en_US.utf8", vec!["a", "1", "A"]), ["1", "a", "A"]);
        // `ab`, `a-b` and `a b` share a primary weight, so their relative
        // order is quaternary (the residual). What holds regardless: all
        // three sort before `aB`, because case is the tertiary weight and
        // it outranks punctuation's.
        let g = sorted("en_US.utf8", vec!["aB", "ab", "a-b", "a b"]);
        assert_eq!(g[3], "aB", "case outranks punctuation: {g:?}");
        // No numeric-aware ordering: a10 sorts between a1 and a2.
        assert_eq!(sorted("en_US.utf8", vec!["a2", "a10", "a1"]), ["a1", "a10", "a2"]);
        // Latin, then Kana, then Han.
        assert_eq!(sorted("en_US.utf8", vec!["中", "あ", "z", "Z"]), ["z", "Z", "あ", "中"]);
    }

    /// C and POSIX are byte order, and are answered without ICU.
    #[test]
    fn c_and_posix_are_byte_order() {
        for c in ["C", "POSIX", "c", "posix"] {
            assert_eq!(compare(c, "B", "a"), Some(Ordering::Less), "{c}");
            assert_eq!(sorted(c, vec!["a", "B", "_"]), ["B", "_", "a"], "{c}");
        }
    }

    /// PG spells locales the POSIX way. All of these name the same thing.
    #[test]
    fn posix_locale_spellings_are_understood() {
        for name in ["en_US", "en_US.utf8", "en_US.UTF-8", "en-US"] {
            assert!(is_supported(name), "{name}");
            assert_eq!(compare(name, "a", "A"), Some(Ordering::Less), "{name}");
        }
    }

    /// A name this build cannot perform answers None rather than silently
    /// substituting another collation.
    #[test]
    fn an_unknown_name_declines_instead_of_guessing() {
        assert_eq!(compare("no_such_locale_at_all", "a", "b"), None);
        assert!(!is_supported("no_such_locale_at_all"));
    }

    /// v7.39 (round 683) — word-initial punctuation, which is where the
    /// round-678 rules were incomplete. Measured on PG18: `_under` sorts
    /// between `cherry` and `Zebra`, i.e. as `under`, because punctuation is
    /// variable-weighted — ignorable at the primary level and only a
    /// tiebreak once the letters agree. The round-678 probe used single
    /// characters, where every primary weight IS equal, so it only ever saw
    /// the degenerate case and recorded it as "space < underscore < digit
    /// < letter".
    /// Punctuation is ignorable at the primary level, so a value sorts by
    /// its letters. All PG18-measured.
    #[test]
    fn punctuation_is_variable_weighted_like_pg() {
        assert_eq!(
            sorted("en_US.utf8", vec!["_under", "apple", "Zebra", "cherry"]),
            ["apple", "cherry", "_under", "Zebra"]
        );
        assert_eq!(
            sorted("en_US.utf8", vec!["_id", "name", "zip", "_ts", "email"]),
            ["email", "_id", "name", "_ts", "zip"]
        );
        assert_eq!(
            sorted("en_US.utf8", vec!["O'Brien", "Oakes", "Obrien", "O-Brien"]),
            ["Oakes", "Obrien", "O'Brien", "O-Brien"]
        );
        // `de luca` / `de-luca` / `deluca` all reduce to `deluca` at the
        // primary level, so their RELATIVE order is a quaternary question —
        // the residual below. What is asserted is that they group together
        // ahead of `demarco`, which is the property the collation buys.
        let g = sorted("en_US.utf8", vec!["de luca", "deluca", "de-luca", "demarco"]);
        assert_eq!(g[3], "demarco", "the de-luca variants group before demarco: {g:?}");
    }

    /// The one case that stays wrong, pinned as wrong so it is not mistaken
    /// for correct. A value that is ENTIRELY punctuation has no letters, so
    /// every primary weight ties and the answer comes from quaternary
    /// weights, where PG and ICU differ. PG gives ` , -, ., _`.
    ///
    /// Swept all five alternate-handling / max_variable combinations: the
    /// two that get this right get every case with a letter in it wrong.
    /// F36 residual.
    #[test]
    fn all_punctuation_values_now_match_pg() {
        // Round 683/684 recorded this shape as F36's residual; round 690's
        // deterministic tiebreak closed it, because ICU calls these Equal at
        // every level and PG's answer here is byte order. Re-measured on
        // PG18 (`' -._`) before this pin changed.
        assert_eq!(
            sorted("en_US.utf8", vec!["_", " ", "-", "."]),
            [" ", "-", ".", "_"]
        );
    }

    /// Different locales really do disagree — otherwise this whole
    /// dependency would be buying nothing. Swedish sorts å after z; English
    /// treats it as an a.
    #[test]
    fn a_locale_tailoring_actually_changes_the_answer() {
        assert_eq!(compare("sv_SE.utf8", "z", "å"), Some(Ordering::Less));
        assert_eq!(compare("en_US.utf8", "z", "å"), Some(Ordering::Greater));
    }

    /// v7.39 (round 680) — how much of PG18's collation list this build can
    /// actually perform, measured rather than promised.
    ///
    /// The RFC left "which collations" open. This answers it with a number
    /// and, more usefully, prints the ones that fail, so the next person
    /// knows what a customer's dump can name that SPG would only record.
    #[test]
    fn survey_pg18_collation_coverage() {
        let all = super::survey::PG18_COLLATIONS;
        let unsupported: Vec<&str> =
            all.iter().copied().filter(|n| !is_supported(n)).collect();
        let supported = all.len() - unsupported.len();
        assert_eq!(
            unsupported,
            Vec::<&str>::new(),
            "{supported}/{} performable",
            all.len()
        );
    }
}
