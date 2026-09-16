//! 8.0.3 — `CREATE EXTENSION` and `DROP EXTENSION` install and remove.
//!
//! Both statements only checked a name against the list of extensions this
//! build provides, and `pg_extension` listed that whole list. So a database
//! that created no extension reported five, and `pg_dump` wrote a
//! `CREATE EXTENSION` for each — which PostgreSQL then refused for `vector`
//! (not installed there) and for `pgcrypto` in `pg_catalog`
//! (`gen_random_uuid` already exists). sentori's §4.4.
//!
//! An extension this build does not provide is still accepted with a
//! WARNING (round 697's resolution: refusing would stop a dump restoring
//! into SPG), and is recorded like any other — the database did ask for
//! it, and a dump taken from here restores it into PostgreSQL, where it
//! can be installed.

extern crate alloc;

use alloc::format;
use alloc::string::String;

use crate::{Engine, EngineError};

/// Every database has PL/pgSQL; PG lists it in `pg_extension` without
/// anyone creating it, in `pg_catalog`.
pub(crate) const ALWAYS_INSTALLED: &str = "plpgsql";

/// The flag an empty leading name carries (see `ValidateOnlyKind`).
fn split_flag(names: &[String]) -> (bool, &[String]) {
    match names.split_first() {
        Some((first, rest)) if first.is_empty() => (true, rest),
        _ => (false, names),
    }
}

impl Engine {
    /// `CREATE EXTENSION [IF NOT EXISTS] e [SCHEMA s]`. `true` when the
    /// catalog changed.
    pub(crate) fn exec_create_extension(&mut self, names: &[String]) -> Result<bool, EngineError> {
        let (if_not_exists, operands) = split_flag(names);
        let Some(name) = operands.first() else {
            return Ok(false);
        };
        let installed = name.eq_ignore_ascii_case(ALWAYS_INSTALLED)
            || self.active_catalog().extensions().contains_key(name);
        if installed {
            let message = format!("extension \"{name}\" already exists");
            if if_not_exists {
                self.notice(format!("{message}, skipping"));
                return Ok(false);
            }
            return Err(EngineError::Unsupported(message));
        }
        // PG installs into the schema named, else the first schema on the
        // search path that exists — `public` in every SPG session.
        let schema = operands.get(1).map_or("public", String::as_str);
        if !self.active_catalog().schema_exists(schema) {
            return Err(EngineError::Unsupported(format!(
                "schema \"{schema}\" does not exist"
            )));
        }
        if !crate::system_catalog::INSTALLED_EXTENSIONS
            .iter()
            .any(|(e, _)| e.eq_ignore_ascii_case(name))
        {
            self.warning(format!(
                "extension \"{name}\" is not provided by this build; SPG \
                 accepts the statement so a dump restores, but nothing \
                 that extension supplies will be available"
            ));
        }
        let (name, schema) = (name.clone(), String::from(schema));
        let changed = self.active_catalog_mut().install_extension(&name, &schema);
        Ok(changed && self.catalog_change_is_committed())
    }

    /// `DROP EXTENSION [IF EXISTS] e [, …]`. `true` when the catalog
    /// changed. PG checks every name before dropping any, so a missing one
    /// without `IF EXISTS` drops nothing.
    pub(crate) fn exec_drop_extension(&mut self, names: &[String]) -> Result<bool, EngineError> {
        let (if_exists, operands) = split_flag(names);
        let mut present: alloc::vec::Vec<String> = alloc::vec::Vec::new();
        for name in operands {
            // PG drops PL/pgSQL when nothing depends on it (measured). SPG's
            // PL/pgSQL is not an installed extension but the engine itself,
            // so the statement succeeds, says so, and the language stays.
            if name.eq_ignore_ascii_case(ALWAYS_INSTALLED) {
                self.warning(format!(
                    "extension \"{name}\" is built into SPG and was not removed"
                ));
                continue;
            }
            if self.active_catalog().extensions().contains_key(name) {
                present.push(name.clone());
            } else if if_exists {
                self.notice(format!("extension \"{name}\" does not exist, skipping"));
            } else {
                return Err(EngineError::Unsupported(format!(
                    "extension \"{name}\" does not exist"
                )));
            }
        }
        let mut changed = false;
        for name in present {
            changed |= self.active_catalog_mut().uninstall_extension(&name);
        }
        Ok(changed && self.catalog_change_is_committed())
    }
}
