//! r1043 — which built-in functions are IMMUTABLE, as a positive list.
//!
//! Round 597 drew its allowlist of node kinds without function calls, and
//! wrote down why: "a function whose volatility SPG cannot look up". This
//! is that lookup, and it is a list of what has been checked rather than
//! a list of exceptions.
//!
//! The distinction is the whole point. `system_catalog`'s `volatile_names`
//! names seven functions and renders everything else as `provolatile='i'`
//! for `pg_proc`. That is fine for a catalog column a human reads and
//! fatal as a folding gate: a function nobody classified would be folded
//! by DEFAULT, which is how `random()` becomes one draw shared by every
//! row. A positive list fails the other way — an immutable function that
//! is not here is merely not folded, and stays exactly as fast as it was.
//!
//! ## What earns a place
//!
//! The value depends only on the arguments. Not on the catalog, not on
//! the session (`search_path`, `TimeZone`, `lc_*`), not on the clock, not
//! on a sequence, not on a random source. PostgreSQL's own `provolatile`
//! is the reference and every entry below matches `'i'` there, checked
//! against 18.4 with:
//!
//! ```sql
//! SELECT proname, provolatile FROM pg_proc WHERE proname = ANY($1);
//! ```
//!
//! Deliberately absent, and each for a stated reason rather than an
//! oversight:
//!
//! * `now`, `current_timestamp`, `clock_timestamp`, `random`,
//!   `gen_random_uuid`, `nextval`, `currval` — volatile or stable-only.
//! * `to_char`, `to_timestamp`, `to_date` — PG marks these STABLE, not
//!   immutable: they read `DateStyle` / `lc_time` / `TimeZone`.
//! * `lower`, `upper`, `initcap` — PG marks them immutable, and SPG's
//!   own implementations consult the session's collation. Until that is
//!   separated they read state this pass cannot supply.
//!
//! Five names were on the first draft of the list and came off it when
//! the query above was actually run, which is the reason to run it:
//!
//! * `concat`, `concat_ws` — PG says `s`. They call the argument types'
//!   output functions, and an output function may be stable.
//! * `length`, `quote_literal`, `quote_nullable` — PG has both `i` and
//!   `s` overloads under each name. A name that is immutable in one
//!   signature and stable in another cannot be cleared by name alone.
//! * `coalesce`, `nullif`, `greatest`, `least`, `trim` — not functions in
//!   PG at all (SQL constructs, and `trim` is `btrim`), so `pg_proc` has
//!   nothing to check them against. `greatest` and `least` over text
//!   would read a collation anyway.

/// Whether this built-in function's value depends only on its arguments.
///
/// The name is matched case-insensitively, as SQL identifiers are.
#[must_use]
pub(crate) fn is_immutable_builtin(name: &str) -> bool {
    // Sorted by family so a reader can see what is covered and what is
    // conspicuously not.
    const IMMUTABLE: &[&str] = &[
        // Binary / text encoding. `decode(lpad(to_hex(id),16,'0'),'hex')`
        // is the shape that motivated this round: 337 ms against 0.34 for
        // PostgreSQL on a 400,000-row table, because it was recomputed
        // once per row.
        "decode",
        "encode",
        "to_hex",
        "md5",
        "sha224",
        "sha256",
        "sha384",
        "sha512",
        // Text shaping that does not read a collation.
        "lpad",
        "rpad",
        "ltrim",
        "rtrim",
        "btrim",
        "repeat",
        "reverse",
        "left",
        "right",
        "substr",
        "substring",
        "replace",
        "translate",
        "split_part",
        "strpos",
        "position",
        "char_length",
        "character_length",
        "octet_length",
        "bit_length",
        "ascii",
        "chr",
        "quote_ident",
        // Arithmetic.
        "abs",
        "ceil",
        "ceiling",
        "floor",
        "round",
        "trunc",
        "sign",
        "mod",
        "div",
        "power",
        "sqrt",
        "cbrt",
        "exp",
        "ln",
        "log",
        "log10",
        "pi",
        "degrees",
        "radians",
        "gcd",
        "lcm",
        "factorial",
        "width_bucket",
        // Trigonometry — the plain forms; the `d` (degree) variants are
        // immutable in PG too and are listed beside them.
        "sin",
        "cos",
        "tan",
        "cot",
        "asin",
        "acos",
        "atan",
        "atan2",
        "sind",
        "cosd",
        "tand",
        "cotd",
        "asind",
        "acosd",
        "atand",
        "atan2d",
        "sinh",
        "cosh",
        "tanh",
        "asinh",
        "acosh",
        "atanh",
        // Bit and byte manipulation.
        "get_bit",
        "get_byte",
        "set_bit",
        "set_byte",
    ];
    IMMUTABLE.iter().any(|f| f.eq_ignore_ascii_case(name))
}
