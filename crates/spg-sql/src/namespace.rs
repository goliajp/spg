//! 9.0.0 (C9) — a relation's schema, and the key it is stored under.
//!
//! `CREATE SCHEMA` recorded a NAME and nothing else: the qualifier on
//! `sa.t` was dropped at parse time, so `sa.t` and `sb.t` were ONE
//! relation and a two-schema application read the other schema's rows.
//! Measured 2026-09-21 against PostgreSQL 18.6.
//!
//! A relation now belongs to a schema, and the two travel together as
//! one string — the KEY — so every map that is keyed by a relation name
//! (the catalog itself, the dirty set, the sequence / view / index /
//! comment / owner / ACL registries, a foreign key's target, the WAL and
//! the audit log) carries the schema for free.
//!
//! The separator is NUL, which cannot occur in an identifier: PostgreSQL
//! rejects it because its identifiers are C strings, and [the lexer]
//! rejects it here for the same reason. So a key is unambiguous, and a
//! key that reaches a client because some reader forgot to split it
//! arrives visibly broken rather than plausibly wrong.
//!
//! `public` keeps the bare name. A database that never names another
//! schema is keyed exactly as it was before this, which is what lets an
//! existing catalog file load unchanged.
//!
//! [the lexer]: crate::lexer

use alloc::string::{String, ToString};

/// The byte between a schema and a relation name inside a key.
pub const SEP: char = '\0';

/// The schema a relation belongs to when nothing says otherwise.
pub const PUBLIC: &str = "public";

/// The schemas whose qualifier is not part of a relation's identity:
/// `public` is the default one, and the two catalog schemas are answered
/// by the synthesised relations, which have one name each.
#[must_use]
pub fn qualifier_is_implicit(schema: &str) -> bool {
    schema.eq_ignore_ascii_case(PUBLIC)
        || schema.eq_ignore_ascii_case("pg_catalog")
        || schema.eq_ignore_ascii_case("information_schema")
}

/// The key a relation named `name` in `schema` is stored under.
#[must_use]
pub fn qualified_key(schema: &str, name: &str) -> String {
    if qualifier_is_implicit(schema) {
        return name.to_string();
    }
    let mut out = String::with_capacity(schema.len() + 1 + name.len());
    out.push_str(schema);
    out.push(SEP);
    out.push_str(name);
    out
}

/// 9.0.0 (C8) — the key for a relation in a database of its own, one
/// level above the schema. A database made by `CREATE DATABASE` owns its
/// relations; the datadir's own database keeps the shorter key, so
/// nothing that exists today is re-keyed.
#[must_use]
pub fn database_key(database: &str, schema: &str, name: &str) -> String {
    let schema = if qualifier_is_implicit(schema) {
        PUBLIC
    } else {
        schema
    };
    let mut out = String::with_capacity(database.len() + schema.len() + name.len() + 2);
    out.push_str(database);
    out.push(SEP);
    out.push_str(schema);
    out.push(SEP);
    out.push_str(name);
    out
}

/// The database a key belongs to, or `None` for the datadir's own.
#[must_use]
pub fn database_of(key: &str) -> Option<&str> {
    let (first, rest) = key.split_once(SEP)?;
    rest.contains(SEP).then_some(first)
}

/// The schema and the bare name inside a key. An unqualified key belongs
/// to [`PUBLIC`]; a three-part key names its database first.
#[must_use]
pub fn split_key(key: &str) -> (&str, &str) {
    match key.split_once(SEP) {
        Some((first, rest)) => match rest.split_once(SEP) {
            // database, schema, name
            Some((schema, name)) => (schema, name),
            None => (first, rest),
        },
        None => (PUBLIC, key),
    }
}

/// The schema part of a key.
#[must_use]
pub fn schema_of(key: &str) -> &str {
    split_key(key).0
}

/// The bare name inside a key — what a client is shown, and what
/// `pg_class.relname` answers.
#[must_use]
pub fn bare_of(key: &str) -> &str {
    split_key(key).1
}

/// True when the key names a relation outside `public`.
#[must_use]
pub fn is_qualified(key: &str) -> bool {
    key.contains(SEP)
}

/// How a key is written in a sentence a client reads: `sa.t` for a
/// relation in another schema, `t` for one in `public`. PostgreSQL
/// writes the qualifier the same way (`relation "sa.t" does not exist`).
#[must_use]
pub fn display_key(key: &str) -> String {
    let (schema, name) = split_key(key);
    if schema == PUBLIC && !key.contains(SEP) {
        return key.to_string();
    }
    let mut out = String::with_capacity(schema.len() + 1 + name.len());
    out.push_str(schema);
    out.push('.');
    out.push_str(name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_keeps_the_bare_name() {
        assert_eq!(qualified_key("public", "t"), "t");
        assert_eq!(qualified_key("PUBLIC", "t"), "t");
        assert_eq!(qualified_key("pg_catalog", "pg_class"), "pg_class");
    }

    #[test]
    fn another_schema_travels_with_the_name() {
        let k = qualified_key("sa", "t");
        assert_eq!(split_key(&k), ("sa", "t"));
        assert_eq!(display_key(&k), "sa.t");
        assert!(is_qualified(&k));
    }

    #[test]
    fn a_database_key_carries_three_parts() {
        let k = database_key("c8a", "public", "t");
        assert_eq!(database_of(&k), Some("c8a"));
        assert_eq!(split_key(&k), ("public", "t"));
        assert_eq!(display_key(&k), "public.t");
        let s = database_key("c8a", "sa", "t");
        assert_eq!(database_of(&s), Some("c8a"));
        assert_eq!(split_key(&s), ("sa", "t"));
        // A two-part key is a schema, not a database.
        let two = qualified_key("sa", "t");
        assert_eq!(database_of(&two), None);
    }

    #[test]
    fn a_name_with_a_dot_in_it_is_not_a_qualified_key() {
        // `CREATE TABLE "a.b"` is one relation named `a.b` in the search
        // path, which is why the separator is not a dot.
        let k = qualified_key("public", "a.b");
        assert_eq!(split_key(&k), ("public", "a.b"));
        assert!(!is_qualified(&k));
    }
}
