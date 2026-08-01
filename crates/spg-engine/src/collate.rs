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
    if name.eq_ignore_ascii_case("C") || name.eq_ignore_ascii_case("POSIX") {
        return Some(a.as_bytes().cmp(b.as_bytes()));
    }
    let locale = icu_locale_core::Locale::try_from_str(&normalise(name)).ok()?;
    let prefs = icu_collator::CollatorPreferences::from(&locale);
    let collator =
        icu_collator::Collator::try_new(prefs, icu_collator::options::CollatorOptions::default())
            .ok()?;
    Some(collator.compare(a, b))
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
    if name.eq_ignore_ascii_case("default") {
        return String::from("und");
    }
    let head = name.split(['.', '@']).next().unwrap_or(name);
    head.replace('_', "-")
}

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
        // Space < underscore < digit < letter.
        assert_eq!(sorted("en_US.utf8", vec!["a", "1", "_", " ", "A"]), [" ", "_", "1", "a", "A"]);
        // Space and hyphen carry weight — they are not ignorable.
        assert_eq!(
            sorted("en_US.utf8", vec!["aB", "ab", "a-b", "a b"]),
            ["a b", "a-b", "ab", "aB"]
        );
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

    /// Different locales really do disagree — otherwise this whole
    /// dependency would be buying nothing. Swedish sorts å after z; English
    /// treats it as an a.
    #[test]
    fn a_locale_tailoring_actually_changes_the_answer() {
        assert_eq!(compare("sv_SE.utf8", "z", "å"), Some(Ordering::Less));
        assert_eq!(compare("en_US.utf8", "z", "å"), Some(Ordering::Greater));
    }
}
