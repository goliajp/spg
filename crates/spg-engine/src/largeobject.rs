//! v7.39 (round 342+, V40) — the two large-object calls that touch a
//! SERVER FILE.
//!
//! `lo_import('/path')` and `lo_export(oid, '/path')` are the only members
//! of the lo_* family that do file IO, and the engine is `no_std` — it has
//! no filesystem. The rest of the family (round 306's descriptor table,
//! `lo_get` / `lo_put` / `lo_from_bytea` / `lo_unlink`) works entirely in
//! the catalog and needs nothing from the host.
//!
//! So these two follow the contract `COPY … FROM '<file>'` already uses
//! (round 249): the shape and every message live here, in the engine, and
//! each host — the server and the embedded API — supplies only the
//! `std::fs` call. That keeps the two hosts saying the same thing.
//!
//! PG 18.4, measured:
//!   * `lo_import` answers the new oid in a column named `lo_import`;
//!     `lo_export` answers `1` in a column named `lo_export`.
//!   * a missing input file is
//!     `could not open server file "/x": No such file or directory`;
//!   * an unwritable target is
//!     `could not create server file "/x": No such file or directory`;
//!   * both are superuser-only: `permission denied for function lo_import`.

use alloc::string::String;

/// A `SELECT lo_import(…)` / `SELECT lo_export(…)` the host must run,
/// because it reads or writes a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoFileCall {
    /// `lo_import('<path>' [, <oid>])`
    Import { path: String, oid: Option<u32> },
    /// `lo_export(<oid>, '<path>')`
    Export { oid: u32, path: String },
}

impl LoFileCall {
    /// The result column name PG uses — its own function name.
    #[must_use]
    pub const fn column_name(&self) -> &'static str {
        match self {
            Self::Import { .. } => "lo_import",
            Self::Export { .. } => "lo_export",
        }
    }
}

/// Recognise a bare `SELECT lo_import(…)` / `SELECT lo_export(…)`.
///
/// Deliberately narrow: only the statement-level spelling is intercepted,
/// which is the one that reaches a file. Anything else — the call nested
/// in a larger expression — is left to the ordinary evaluator, which
/// reports it as unsupported rather than silently doing nothing.
#[must_use]
pub fn parse_lo_file_call(sql: &str) -> Option<LoFileCall> {
    let t = sql.trim().trim_end_matches(';').trim();
    let rest = strip_prefix_ci(t, "select")?.trim_start();
    let (name, args) = split_call(rest)?;
    let args = split_args(args);
    match name.as_str() {
        "lo_import" => match args.as_slice() {
            [p] => Some(LoFileCall::Import {
                path: string_literal(p)?,
                oid: None,
            }),
            [p, o] => Some(LoFileCall::Import {
                path: string_literal(p)?,
                oid: Some(o.trim().parse().ok()?),
            }),
            _ => None,
        },
        "lo_export" => match args.as_slice() {
            [o, p] => Some(LoFileCall::Export {
                oid: o.trim().parse().ok()?,
                path: string_literal(p)?,
            }),
            _ => None,
        },
        _ => None,
    }
}

/// PG's wording for a file it could not read.
#[must_use]
pub fn could_not_open(path: &str, os_error: &str) -> String {
    alloc::format!(
        "could not open server file \"{path}\": {}",
        trim_os(os_error)
    )
}

/// PG's wording for a file it could not write.
#[must_use]
pub fn could_not_create(path: &str, os_error: &str) -> String {
    alloc::format!(
        "could not create server file \"{path}\": {}",
        trim_os(os_error)
    )
}

/// PG's wording when the caller is not a superuser.
#[must_use]
pub fn permission_denied(call: &LoFileCall) -> String {
    alloc::format!("permission denied for function {}", call.column_name())
}

/// std renders an io::Error as `No such file or directory (os error 2)`;
/// PG prints only the message.
fn trim_os(os_error: &str) -> &str {
    os_error.split(" (os error").next().unwrap_or(os_error)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// `name ( args )` → (lower-cased name, the text between the parens).
fn split_call(s: &str) -> Option<(String, &str)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close < open {
        return None;
    }
    let name = s[..open].trim().to_ascii_lowercase();
    if !s[close + 1..].trim().is_empty() {
        return None;
    }
    Some((name, &s[open + 1..close]))
}

/// Split on commas that are not inside a quoted literal.
fn split_args(s: &str) -> alloc::vec::Vec<&str> {
    let mut out = alloc::vec::Vec::new();
    let mut start = 0usize;
    let mut in_quote = false;
    for (i, c) in s.char_indices() {
        match c {
            '\'' => in_quote = !in_quote,
            ',' if !in_quote => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if !s[start..].trim().is_empty() || !out.is_empty() {
        out.push(&s[start..]);
    }
    out
}

/// `'text'` → `text`, with PG's doubled-quote escape.
fn string_literal(s: &str) -> Option<String> {
    let t = s.trim();
    let inner = t.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn recognises_both_calls() {
        assert_eq!(
            parse_lo_file_call("SELECT lo_import('/tmp/a.txt')"),
            Some(LoFileCall::Import {
                path: "/tmp/a.txt".to_string(),
                oid: None
            })
        );
        assert_eq!(
            parse_lo_file_call("select LO_IMPORT('/tmp/a.txt', 4242);"),
            Some(LoFileCall::Import {
                path: "/tmp/a.txt".to_string(),
                oid: Some(4242)
            })
        );
        assert_eq!(
            parse_lo_file_call("SELECT lo_export(4242, '/tmp/b.bin')"),
            Some(LoFileCall::Export {
                oid: 4242,
                path: "/tmp/b.bin".to_string()
            })
        );
    }

    #[test]
    fn leaves_everything_else_alone() {
        for sql in [
            "SELECT lo_get(1)",
            "SELECT 1",
            "SELECT lo_import('/tmp/a') FROM t",
            "SELECT length(lo_import('/tmp/a'))",
            "INSERT INTO t VALUES (lo_import('/tmp/a'))",
        ] {
            assert_eq!(parse_lo_file_call(sql), None, "for `{sql}`");
        }
    }

    #[test]
    fn a_path_may_hold_a_comma_or_a_quote() {
        assert_eq!(
            parse_lo_file_call("SELECT lo_import('/tmp/a,b.txt')"),
            Some(LoFileCall::Import {
                path: "/tmp/a,b.txt".to_string(),
                oid: None
            })
        );
        assert_eq!(
            parse_lo_file_call("SELECT lo_import('/tmp/it''s.txt')"),
            Some(LoFileCall::Import {
                path: "/tmp/it's.txt".to_string(),
                oid: None
            })
        );
    }
}
