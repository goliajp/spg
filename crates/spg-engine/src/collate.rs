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
    // v7.38.13 — MySQL's byte-order collations, for the same reason `C` is
    // answered here: they ARE byte order by definition, so handing them to
    // ICU would be asking it to perform something it has no locale for.
    // `binary` is the `binary` charset's collation; every charset also
    // publishes a `_bin` variant (`utf8mb4_bin`, `latin1_bin`, ...) which
    // compares by CODE POINT — and UTF-8 byte order is code-point order,
    // so the two coincide. They are NO PAD, which a byte compare gives.
    //
    // Without this the name reached `Locale::try_from_str`, failed to
    // parse, and `compare` returned None — which callers read as "this
    // build cannot perform it" and dropped, leaving `ORDER BY` on a
    // `COLLATE utf8mb4_bin` column sorting case-INSENSITIVELY. MySQL
    // 9.7.1 answers `A, Bar, a, bar`; SPG answered `a, A, bar, Bar`.
    if base.eq_ignore_ascii_case("binary")
        || base
            .rsplit_once('_')
            .is_some_and(|(_, tail)| tail.eq_ignore_ascii_case("bin"))
    {
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

/// v7.38.18 (S0) — the ICU sort key for `s` under `collation`, as bytes
/// that compare the way [`compare`] compares the strings themselves.
///
/// This is what lets a B-tree index carry a collation without the B-tree
/// knowing about one. `spg-storage` is `no_std` and holds no collator;
/// it orders `IndexKey` by the derived `Ord`. Handing it a sort key
/// makes that byte comparison the collation's comparison.
///
/// The original string rides along after a NUL for the same reason
/// [`compare`] falls back to it: PG's locale collations are
/// deterministic, so two canonically-equivalent strings that ICU calls
/// equal are still ordered by their bytes, and an index must agree with
/// the scan on that too.
///
/// `None` for a collation this build cannot perform, which callers read
/// as "do not build a collated key" rather than as "these are equal".
pub(crate) fn sort_key(collation: &str, s: &str) -> Option<alloc::vec::Vec<u8>> {
    let name = collation.trim();
    let base = name.split(['.', '@']).next().unwrap_or(name);
    if base.eq_ignore_ascii_case("C")
        || base.eq_ignore_ascii_case("POSIX")
        || base.eq_ignore_ascii_case("binary")
        || base
            .rsplit_once('_')
            .is_some_and(|(_, tail)| tail.eq_ignore_ascii_case("bin"))
    {
        return None;
    }
    // v7.38.19 — resolve through `Collated`, which is where the collator
    // lives, instead of parsing the locale and building a fresh one on
    // every call. Both callers are per-ROW: one builds an index entry,
    // the other an index probe, so this ran once per row of a collated
    // index. It is the same defect the scan filter had -- `Collated`
    // exists for exactly this and two more paths were never connected to
    // it.
    Collated::resolve(name)?.sort_key_of(s)
}

/// v7.38.18 — a collation resolved ONCE, for a comparator that will be
/// called millions of times.
///
/// [`compare`] takes a NAME, and building the collator behind that name
/// costs about ten times what the comparison does: measured over
/// 100,000 comparisons, 52.9 ms building per call against 5.2 ms with
/// one built in advance. A 400,000-row two-key `ORDER BY` builds
/// millions of them, and the release sweep saw the whole cell go from
/// matching PostgreSQL 18.4 to losing 1.5x when database-level
/// collations arrived and that sort started taking this path.
///
/// So the sort resolves each key's collation before it starts and
/// carries this instead of the name. `None` inside means byte order,
/// which is `C` and the `_bin` family — they need no collator and this
/// keeps the branch out of the inner loop.
pub(crate) struct Collated {
    collator: Option<icu_collator::CollatorBorrowed<'static>>,
    /// v7.38.19 — this collation orders `[0-9a-z]` the way bytes do.
    ///
    /// Not a property of collations in general, and not one that can be
    /// asked of a collator: it depends on the locale's tailoring.
    /// Measured against PostgreSQL 18's ICU collations over all 839,160
    /// ordered pairs of two-character strings from that alphabet:
    ///
    /// ```text
    ///   en  0     cs    198     (`ch` is a letter, after `h`)
    ///   sv  0     et  7,992     (`z` sorts between `s` and `t`)
    ///   de  0     lt 20,609     (`y` sorts after `i`)
    ///   fr  0     da    925     (`aa` is `å`, after `z`)
    ///   tr  0     hu     18     (`cs`, `gy`, `sz` … are letters)
    /// ```
    ///
    /// So it is an allowlist, keyed on the language, and the test beside
    /// it re-derives the whole thing in process: every allowed language
    /// must agree over the subset at one, two and three characters, and
    /// every language in the second column above must NOT — an allowlist
    /// nothing falls outside is not a check.
    ascii_byte_order: bool,
}

impl Collated {
    /// `None` when this build cannot perform the name, which leaves the
    /// caller on byte order — the same answer [`compare`] gives.
    pub(crate) fn resolve(name: &str) -> Option<Self> {
        if is_byte_wise(name) {
            return Some(Self {
                collator: None,
                ascii_byte_order: true,
            });
        }
        let normalised = normalise(name);
        let locale = icu_locale_core::Locale::try_from_str(&normalised).ok()?;
        let prefs = icu_collator::CollatorPreferences::from(&locale);
        let collator = icu_collator::Collator::try_new(prefs, pg_options()).ok()?;
        Some(Self {
            collator: Some(collator),
            ascii_byte_order: ascii_alnum_is_byte_order(&normalised),
        })
    }

    /// Whether `[0-9a-z]` orders by bytes under this collation.
    pub(crate) const fn ascii_byte_order(&self) -> bool {
        self.ascii_byte_order
    }

    /// v7.38.19 — the ICU sort key for a value, computed ONCE, so a sort
    /// compares bytes instead of calling the collator per comparison.
    ///
    /// A symbolicated profile of `ORDER BY` over 200,000 collated text
    /// values put **97 % of the non-waiting samples inside ICU** —
    /// `CollatorBorrowed::compare` at 936 and `CollationElements` at
    /// 1,003 against `cmp_multi_key_in`'s 57. A sort of n rows makes
    /// about n·log₂n comparisons — 3.5 million here — where a key needs
    /// n computations.
    ///
    /// The bytes end with a NUL and then the original string, so two
    /// values the collation calls equal still order deterministically,
    /// exactly as [`Collated::compare`] tiebreaks.
    ///
    /// `None` when this collation needs no collator: `C` and the `_bin`
    /// family are byte order already, and building a key for them would
    /// be a copy that changes nothing.
    pub(crate) fn sort_key_of(&self, s: &str) -> Option<alloc::vec::Vec<u8>> {
        let c = self.collator.as_ref()?;
        let mut key: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        c.write_sort_key_to(s, &mut key).ok()?;
        key.push(0);
        key.extend_from_slice(s.as_bytes());
        Some(key)
    }

    /// The same ordering [`compare`] produces, tiebreak included.
    pub(crate) fn compare(&self, a: &str, b: &str) -> Ordering {
        match &self.collator {
            // v7.38.19 — the ASCII shortcut, and it is the whole point
            // of `ascii_byte_order`.
            //
            // A collated comparison of two 192-character strings costs
            // about 1.6 µs; the bytes cost single-digit nanoseconds. On
            // 400,000 rows of hex text that was the difference between
            // 1329.5 ms and PostgreSQL 18's 11.0 on the same query under
            // the same collation.
            //
            // Both operands are tested, so a mixed column stays correct:
            // a value with a space or a capital in it takes the collator,
            // as it must, and only pays the check.
            Some(c) => {
                if self.ascii_byte_order && is_ascii_alnum_lower(a) && is_ascii_alnum_lower(b) {
                    return a.as_bytes().cmp(b.as_bytes());
                }
                // NOTE: this test walks both strings, so it belongs
                // where a value is seen ONCE. `build_order_keys_bound`
                // asks it per value; a sort asks this function per
                // COMPARISON, n·log₂n of them, and on 192-character
                // values that scanning was itself the cost. See
                // `is_ascii_alnum_lower`'s own note.
                // PG's locale collations are deterministic, so a collation
                // tie is broken by the bytes — see `compare`.
                c.compare(a, b).then_with(|| a.as_bytes().cmp(b.as_bytes()))
            }
            None => a.as_bytes().cmp(b.as_bytes()),
        }
    }
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

/// v7.38.14 — is this collation BYTE-WISE?
///
/// The same test `compare` applies before it reaches ICU, exposed so that
/// callers deciding whether to fold ask one question with one answer
/// rather than re-deriving the name rules each time.
/// How a text comparison behaves under one collation: two independent
/// bits that no single flag can express.
///
/// v7.38.18 — `utf8mb4_bin` is byte-wise AND `PAD SPACE`;
/// `utf8mb4_0900_ai_ci` folds case and does NOT pad;
/// `utf8mb4_0900_bin` does neither. Deciding them together, once, is
/// what keeps the comparison path and the compiled path from
/// disagreeing — which is the shape of most of what v7.38.13 through
/// v7.38.18 were spent on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextCompare {
    /// Fold case and accents.
    pub fold_case: bool,
    /// Trailing spaces do not count.
    pub pads: bool,
    /// v7.38.18 (S2) — the LOCALE this comparison orders under, when it
    /// is not byte order.
    ///
    /// Fold and pad are the two bits this struct carried; ordering is a
    /// third thing and it was missing. Without it a scan filter compared
    /// `x < 'b'` by bytes while `ORDER BY x` over the same column
    /// compared by the locale — the exact disagreement between the
    /// interpreted and compiled paths this type's own comment says it
    /// exists to prevent.
    pub order: Option<alloc::string::String>,
}

impl TextCompare {
    /// Does this collation change a comparison at all? A byte-wise,
    /// non-padding collation is plain `memcmp` and needs no fold step.
    pub fn is_plain_bytes(&self) -> bool {
        !self.fold_case && !self.pads && self.order.is_none()
    }

    /// Compare two strings under it, or `None` when this collation adds
    /// no ordering of its own and the caller's byte comparison stands.
    pub fn compare(&self, a: &str, b: &str) -> Option<core::cmp::Ordering> {
        compare(self.order.as_deref()?, a, b)
    }
}

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
pub(crate) fn pads_space(collation: Option<&str>) -> bool {
    let Some(name) = collation else {
        return false;
    };
    let n = name.trim();
    if n.eq_ignore_ascii_case("binary") {
        return false;
    }
    let lower = n.to_ascii_lowercase();
    !(lower.contains("_0900_") || lower.contains("nopad"))
}

pub(crate) fn is_byte_wise(collation: &str) -> bool {
    // v7.38.18 (S0) — one owner, and it is storage, because storage has
    // to ask the same question about an index's column.
    spg_storage::collation_is_byte_wise(collation)
}

/// v7.38.18 (G2) — is there a collation by this name at all?
///
/// PostgreSQL validates against its catalogue and answers `collation
/// "x" for encoding "UTF8" does not exist` for a name that is not in
/// it. SPG could not: it handed the name to ICU, which falls back to
/// the root collation for any well-formed language tag, so `zz_ZZ` and
/// `kl_KL.no_such` were accepted as collations and silently ordered by
/// root. The catalogue in `collation_catalog` is what makes the
/// distinction possible.
///
/// The set is SPG's, not PostgreSQL's, and that is deliberate. MySQL's
/// names (`utf8mb4_0900_ai_ci` and family) are not in PostgreSQL's
/// catalogue and PG 18.4 rejects them — but SPG HAS those collations,
/// on the same database, reachable through the MySQL wire. Refusing
/// them to a PostgreSQL session would mean one database where a column
/// can be declared through one wire and not the other, which is a
/// divergence of its own. So the question is "does SPG have a collation
/// by this name", and the answer is a superset of PostgreSQL's — never
/// a wrong answer, only a name PG would not have accepted.
pub(crate) fn is_known(name: &str) -> bool {
    let n = name.trim();
    // The ENCODING SUFFIX is spelled differently by different systems
    // and is not part of the collation's identity. macOS exports
    // `LANG=en_US.UTF-8`; PostgreSQL's catalogue holds `en_US.utf8` and
    // `en_US`. Matching the whole string turned the commonest locale on
    // a developer's laptop into "does not exist", and a server started
    // there silently kept `C` — found by running the server, which an
    // in-process test could not have shown.
    //
    // The base is what identifies a collation, which is also what
    // `normalise` hands to ICU. `zz_ZZ` still fails: its base is in the
    // catalogue no more than the whole name is.
    fn base(s: &str) -> &str {
        let t = s.trim();
        t.split(['.', '@']).next().unwrap_or(t)
    }
    let want = base(n);
    if crate::collation_catalog::PG_COLLATIONS
        .iter()
        .any(|(_, c, ..)| c.eq_ignore_ascii_case(n) || base(c).eq_ignore_ascii_case(want))
    {
        return true;
    }
    // A POSIX locale name against the ICU half of the catalogue.
    //
    // The libc half is whatever locales the HOST that produced the
    // catalogue had installed — five entries, all English. Checking a
    // German customer's `de_DE.UTF-8` against those refused a collation
    // this build performs perfectly well, which is the catalogue being
    // used for a question it cannot answer. The ICU half does not have
    // that problem: `de-DE-x-icu`, `ja-JP-x-icu` and 869 more come from
    // ICU rather than from the host, so they are the same wherever
    // PostgreSQL is built with ICU.
    //
    // `normalise` already turns `de_DE.UTF-8` into the BCP-47 `de-DE`.
    // `zz_ZZ` still fails: there is no `zz-ZZ-x-icu` and no `zz-x-icu`,
    // which is exactly the difference between a locale ICU has data for
    // and one it merely falls back to root for.
    let tag = normalise(n);
    let lang: alloc::string::String = tag.split('-').next().unwrap_or(&tag).into();
    if crate::collation_catalog::PG_COLLATIONS
        .iter()
        .filter(|(_, _, provider, ..)| *provider == "i")
        .any(|(_, c, ..)| {
            c.strip_suffix("-x-icu").is_some_and(|icu| {
                icu.eq_ignore_ascii_case(&tag) || icu.eq_ignore_ascii_case(&lang)
            })
        })
    {
        return true;
    }
    let lower = n.to_ascii_lowercase();
    // SPG's own spellings for the case-insensitive collation. They are
    // not PostgreSQL's and PG would reject them, but SPG accepts them
    // and has since v7.17 — this asks whether SPG HAS a collation by
    // this name, and it does.
    if lower == "case_insensitive" || lower == "nocase" {
        return true;
    }
    lower == "binary" || lower.ends_with("_ci") || lower.ends_with("_cs") || lower.ends_with("_bin")
}

/// The message PostgreSQL 18.4 gives for a name that is not there.
pub(crate) fn unknown_collation_error(name: &str) -> crate::EngineError {
    crate::EngineError::Unsupported(alloc::format!(
        "collation \"{name}\" for encoding \"UTF8\" does not exist"
    ))
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
/// `[0-9a-z]`, the alphabet the shortcut is verified over.
///
/// Digits and lowercase only. Adding an underscore, a hyphen, a dot or a
/// space costs fourteen to twenty-three THOUSAND disagreements out of the
/// same 936,396 pairs, because those are the characters `AlternateHandling::Shifted`
/// treats as variable — the very handling PostgreSQL's ordering needs.
/// Mixing case costs 216 of 630.
///
/// This walks the whole string, so it is asked ONCE PER VALUE, when the
/// sort key is built — never per comparison. A full sort of 400,000
/// 192-character values makes about 7.4 million comparisons; asking
/// there scanned 2.8 GB and cost more than the ICU calls it replaced
/// (1635.5 ms with keys, 1421.2 with the check in the comparator, and
/// the whole point was to get well under both).
pub(crate) const fn is_ascii_alnum_lower(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_digit() || c.is_ascii_lowercase()) {
            return false;
        }
        i += 1;
    }
    true
}

/// The languages whose tailoring leaves `[0-9a-z]` in byte order.
///
/// Keyed on the language subtag alone: a region or a script does not
/// re-tailor the Latin letters among themselves, and the test checks the
/// tagged forms too.
///
/// This is deliberately SHORT. A language absent from it is not wrong,
/// only unaccelerated, and that is the failure direction to prefer:
/// every entry has to be earned by an exhaustive check, while an
/// omission costs a comparison.
fn ascii_alnum_is_byte_order(normalised: &str) -> bool {
    let lang = normalised.split('-').next().unwrap_or(normalised);
    matches!(
        lang,
        "und" | "en" | "de" | "fr" | "es" | "it" | "pt" | "nl" | "sv" | "tr" | "id" | "ms"
    )
}

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

/// Can a byte-keyed B-tree answer a comparison — or an ordering — on
/// this column?
///
/// SPG's index keys are bytes. MySQL's default text collation is not:
/// `utf8mb4_0900_ai_ci` folds case, so `s = 'ALPHA'` finds a row stored
/// as `alpha` and a byte probe for `ALPHA` finds nothing. An index that
/// answers a query the scan would answer differently has changed the
/// ANSWER, which is the one thing an index may never do.
///
/// Measured against MySQL 9.7.1 on a four-row table, before this test
/// existed — same query, same data, index versus no index:
///
/// | | MySQL | no index | indexed |
/// |---|---|---|---|
/// | `s = 'ALPHA'`                  | 1     | 1     | *(none)* |
/// | `s IN ('ALPHA','BETA')`        | 1,2   | 1,2   | *(none)* |
/// | `s BETWEEN 'ALPHA' AND 'DELTA'`| 1,2,4 | 1,2,4 | 2 |
/// | `ORDER BY s LIMIT 2`           | 1,2   | 1,2   | 2,3 |
///
/// on TEXT, VARCHAR and CHAR alike. Answering `false` here costs a scan;
/// the alternative cost rows.
///
/// A column that says nothing about its collation stores
/// [`Collation::Binary`], which is also the struct's default — so this
/// asks the DIALECT first. Under MySQL a plain text column stores
/// `CaseInsensitive` (verified, not assumed) and only a declared
/// `COLLATE utf8mb4_bin` stores `Binary`, which keeps its seek.
pub(crate) fn column_key_is_bytewise(col: &spg_storage::ColumnSchema, mysql: bool) -> bool {
    // Only text folds. Integers, dates and uuids compare by value in
    // every collation there is, and keep their seeks in both dialects.
    if !matches!(
        col.ty,
        spg_storage::DataType::Text
            | spg_storage::DataType::Varchar(_)
            | spg_storage::DataType::Char(_)
    ) {
        return true;
    }
    // A case-insensitive collation folds whoever asks, PG included.
    if matches!(col.collation, spg_storage::Collation::CaseInsensitive) {
        return false;
    }
    // v7.38.18 (S0) — and a DECLARED locale collation is neither byte
    // order nor a case fold, which this asked about for two versions
    // without ever asking about the name.
    //
    // The enum answers whether a column FOLDS. A PG column declared
    // `COLLATE "en_US.utf8"` stores `Collation::Binary` — the struct's
    // default, meaning "nothing was said about folding" — and under the
    // PG dialect `!mysql` made that byte-wise. So the seek ran on byte
    // keys while the predicate meant the locale, and the table above is
    // what that costs. Measured, five rows, `WHERE x > 'b'` on a column
    // declared `en_US.utf8`: PG 18.4 answers `Bob client DateStyle
    // Zebra` with an index and without; SPG answered all four without
    // and `client` with. Three rows gone.
    //
    // Answering `false` sends it back to the scan, which is this
    // function's own stated trade. `docs/DESIGN-2026-08-23-collation.md`
    // S0 gives the key back its seek by carrying the collation IN the
    // key; until that lands, correct and slower.
    !mysql || matches!(col.collation, spg_storage::Collation::Binary)
}

#[cfg(test)]
#[path = "collate_survey.rs"]
mod survey;

#[cfg(test)]
mod tests {
    use super::*;
    /// v7.38.18 (S0 feasibility) — the property the whole
    /// collation-aware index key rests on: comparing two sort keys as
    /// BYTES must give the same answer as `compare` gives on the strings.
    /// If it does not, an index built from sort keys orders differently
    /// from a scan, which is the defect this is meant to close.
    #[test]
    fn sort_key_bytes_order_the_way_compare_does() {
        let words = [
            "apple",
            "Apple",
            "APPLE",
            "Bob",
            "bob",
            "client",
            "DateStyle",
            "Zebra",
            "zebra",
            "_under",
            "cherry",
            "de-luca",
            "deluca",
            "O'Brien",
            "Obrien",
            "résumé",
            "resume",
            "Résumé",
            "1abc",
            "",
            " ",
            "a",
            "A",
            "ä",
            "Ä",
            "z",
            "Z",
            "élan",
            "elan",
        ];
        for coll in ["en_US.utf8", "de_DE.utf8", "fr_FR.utf8"] {
            for a in words {
                for b in words {
                    let by_compare = compare(coll, a, b).expect("supported");
                    let ka = sort_key(coll, a).expect("supported");
                    let kb = sort_key(coll, b).expect("supported");
                    assert_eq!(
                        ka.cmp(&kb),
                        by_compare,
                        "{coll}: {a:?} vs {b:?} — sort keys say {:?}, compare says {by_compare:?}",
                        ka.cmp(&kb)
                    );
                }
            }
        }
    }

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
        assert_eq!(
            sorted("en_US.utf8", vec!["b", "A", "a", "B"]),
            ["a", "A", "b", "B"]
        );
        assert_eq!(compare("en_US.utf8", "a", "A"), Some(Ordering::Less));
        // Accents are SECONDARY: primary treats é as e, so a shorter string
        // wins before the accent is ever consulted.
        assert_eq!(
            sorted("en_US.utf8", vec!["f", "ê", "E", "é", "e"]),
            ["e", "E", "é", "ê", "f"]
        );
        assert_eq!(
            compare("en_US.utf8", "résumé", "resumes"),
            Some(Ordering::Less)
        );
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
        assert_eq!(
            sorted("en_US.utf8", vec!["a2", "a10", "a1"]),
            ["a1", "a10", "a2"]
        );
        // Latin, then Kana, then Han.
        assert_eq!(
            sorted("en_US.utf8", vec!["中", "あ", "z", "Z"]),
            ["z", "Z", "あ", "中"]
        );
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
        let g = sorted(
            "en_US.utf8",
            vec!["de luca", "deluca", "de-luca", "demarco"],
        );
        assert_eq!(
            g[3], "demarco",
            "the de-luca variants group before demarco: {g:?}"
        );
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
        let unsupported: Vec<&str> = all.iter().copied().filter(|n| !is_supported(n)).collect();
        let supported = all.len() - unsupported.len();
        assert_eq!(
            unsupported,
            Vec::<&str>::new(),
            "{supported}/{} performable",
            all.len()
        );
    }
}

#[cfg(test)]
mod ascii_shortcut_tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;

    const ALPHA: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    /// Every string of one, two and three characters over `[0-9a-z]`,
    /// plus a slice of the four-character ones: 36 + 1,296 + 46,656 +
    /// ~1,300.
    fn corpus() -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for &a in ALPHA {
            out.push(String::from_utf8(alloc::vec![a]).unwrap());
            for &b in ALPHA {
                out.push(String::from_utf8(alloc::vec![a, b]).unwrap());
                for &c in ALPHA {
                    out.push(String::from_utf8(alloc::vec![a, b, c]).unwrap());
                    for &d in ALPHA {
                        if (usize::from(a) + usize::from(b) + usize::from(c) + usize::from(d))
                            % 1301
                            == 0
                        {
                            out.push(String::from_utf8(alloc::vec![a, b, c, d]).unwrap());
                        }
                    }
                }
            }
        }
        out
    }

    /// Whether the collation and the bytes agree over the WHOLE corpus.
    ///
    /// Asked by sorting rather than by comparing every pair. Two total
    /// orders agree on every pair exactly when they produce the same
    /// sequence, so one sort answers what 1.2 billion comparisons would,
    /// and answers it for four-character strings too.
    fn first_disagreement(name: &str) -> Option<(String, String)> {
        let c = Collated::resolve(name).expect("resolvable");
        let icu = c.collator.as_ref().expect("a collator");
        let mut by_bytes = corpus();
        by_bytes.sort_unstable();
        let mut by_coll = by_bytes.clone();
        by_coll.sort_by(|x, y| {
            icu.compare(x.as_str(), y.as_str())
                .then_with(|| x.as_bytes().cmp(y.as_bytes()))
        });
        by_bytes
            .iter()
            .zip(by_coll.iter())
            .find(|(a, b)| a != b)
            .map(|(a, b)| (a.clone(), b.clone()))
    }

    /// The allowlist, re-derived. Every language it names must order the
    /// alphabet exactly as bytes do.
    #[test]
    fn every_allowed_collation_orders_the_alphabet_by_bytes() {
        for name in [
            "und",
            "en_US.utf8",
            "en_GB",
            "de_DE.utf8",
            "fr_FR",
            "es_ES",
            "it_IT",
            "pt_BR",
            "nl_NL",
            "sv_SE",
            "tr_TR",
            "id_ID",
            "ms_MY",
            "de_AT",
            "fr_CA",
        ] {
            let c = Collated::resolve(name).expect("resolvable");
            assert!(
                c.ascii_byte_order(),
                "{name} must be on the allowlist for this test to mean anything"
            );
            assert_eq!(first_disagreement(name), None, "{name}");
        }
    }

    /// And the other half, which is what makes the first half a check.
    ///
    /// These five DO retailor the alphabet. PostgreSQL 18's ICU
    /// collations disagree with bytes on 198 (cs), 7,992 (et), 20,609
    /// (lt), 925 (da) and 18 (hu) of the 839,160 two-character pairs. An
    /// allowlist nothing falls outside is not an allowlist.
    #[test]
    fn the_languages_that_retailor_the_alphabet_are_not_allowed() {
        for (name, why) in [
            ("cs_CZ", "`ch` is a letter, sorting after `h`"),
            ("et_EE", "`z` sorts between `s` and `t`"),
            ("lt_LT", "`y` sorts after `i`"),
            ("da_DK", "`aa` is `å`, sorting after `z`"),
            ("hu_HU", "`cs`, `gy`, `sz` … are letters"),
        ] {
            let c = Collated::resolve(name).expect("resolvable");
            assert!(!c.ascii_byte_order(), "{name}: {why}");
            assert!(
                first_disagreement(name).is_some(),
                "{name} was excluded for a reason that does not reproduce: {why}"
            );
        }
    }

    /// The shortcut only fires when BOTH operands are in the alphabet,
    /// so a mixed column keeps the collator where it needs it. Under
    /// `en_US` a capital sorts among the lowercase letters, which bytes
    /// never do.
    #[test]
    fn a_value_outside_the_alphabet_still_takes_the_collator() {
        let c = Collated::resolve("en_US.utf8").expect("resolvable");
        assert!(c.ascii_byte_order());
        assert_eq!(c.compare("Bob", "apple"), Ordering::Greater);
        assert_eq!("Bob".as_bytes().cmp("apple".as_bytes()), Ordering::Less);
        assert_eq!(c.compare("bob", "apple"), Ordering::Greater);
        assert!(!is_ascii_alnum_lower("a b"));
        assert!(is_ascii_alnum_lower("a0z"));
        assert!(!is_ascii_alnum_lower("A"));
        assert!(!is_ascii_alnum_lower("\u{e9}"));
        assert!(is_ascii_alnum_lower(""));
    }
}
