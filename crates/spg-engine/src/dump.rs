//! r1054 (7.38 S3.1, design D27) — the engine dumps itself to SQL.
//!
//! The contract is SELF-consistency, not pg_dump emission fidelity
//! (that campaign is registered separately): `dump → restore into a
//! fresh engine → dump` must be a FIXED POINT, and the restored data
//! must checksum-match the original. Everything here leans on surfaces
//! the engine already answers for — `pg_indexes.indexdef` for index
//! DDL, its own SELECT for data (visibility included), the constfold
//! literal renderer for values — so the dump cannot drift from what
//! the engine itself believes.

use crate::{Engine, EngineError, QueryResult};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

impl Engine {
    /// Serialize every user table (schema, constraints, data), index
    /// and view to SQL the engine itself re-executes.
    ///
    /// # Errors
    /// Storage or introspection failures; a value the literal renderer
    /// cannot express round-trip-safely.
    pub fn dump_sql(&mut self) -> Result<String, EngineError> {
        let mut out = String::from("-- spg dump (self-consistent form)\n");
        let mut tables = self.active_catalog().table_names();
        tables.retain(|t| !t.starts_with("__spg_"));
        tables.sort();

        // ── schema ──────────────────────────────────────────────────
        for name in &tables {
            let Some(t) = self.active_catalog().get(name) else {
                continue;
            };
            let schema = t.schema().clone();
            let mut lines: Vec<String> = Vec::new();
            for c in &schema.columns {
                let mut line = format!("  {} {}", quote_ident(&c.name), ddl_type(c.ty));
                if let Some(e) = &c.user_enum_type {
                    line = format!("  {} {}", quote_ident(&c.name), quote_ident(e));
                }
                // v7.38.18 — the column's collation. It was never
                // emitted, so a column declared `COLLATE "en_US.utf8"`
                // came back byte-ordered after a dump/restore and every
                // `ORDER BY` on it silently changed answer. The
                // dump-compat gate could not see it: both sides of the
                // round trip lost it identically.
                //
                // `C` is skipped because it is what a column with no
                // clause already gets, and PostgreSQL does not print it
                // either.
                if let Some(coll) = &c.collation_name
                    && !coll.eq_ignore_ascii_case("C")
                    && !coll.eq_ignore_ascii_case("default")
                {
                    line.push_str(&format!(" COLLATE {}", quote_ident(coll)));
                }
                if !c.nullable {
                    line.push_str(" NOT NULL");
                }
                if let Some(d) = &c.default_text {
                    line.push_str(&format!(" DEFAULT {d}"));
                }
                lines.push(line);
            }
            for uc in &schema.uniqueness_constraints {
                let cols: Vec<String> = uc
                    .columns
                    .iter()
                    .filter_map(|&p| schema.columns.get(p))
                    .map(|c| quote_ident(&c.name))
                    .collect();
                let kind = if uc.is_primary_key {
                    "PRIMARY KEY"
                } else if uc.nulls_not_distinct {
                    "UNIQUE NULLS NOT DISTINCT"
                } else {
                    "UNIQUE"
                };
                lines.push(format!("  {kind} ({})", cols.join(", ")));
            }
            out.push_str(&format!(
                "CREATE TABLE {} (\n{}\n);\n",
                quote_ident(name),
                lines.join(",\n")
            ));
        }

        // ── data — through the engine's own SELECT, so visibility and
        // rendering are the engine's, not this module's ─────────────
        for name in &tables {
            let rows = match self.execute(&format!("SELECT * FROM {}", quote_ident(name)))? {
                QueryResult::Rows { rows, .. } => rows,
                _ => continue,
            };
            for chunk in rows.chunks(100) {
                let tuples: Vec<String> = chunk
                    .iter()
                    .map(|r| {
                        let vals: Vec<String> = r
                            .values
                            .iter()
                            .map(|v| format!("{}", crate::clock::value_to_literal(v.clone())))
                            .collect();
                        format!("({})", vals.join(", "))
                    })
                    .collect();
                out.push_str(&format!(
                    "INSERT INTO {} VALUES {};\n",
                    quote_ident(name),
                    tuples.join(", ")
                ));
            }
        }

        // ── secondary indexes, via the engine's own pg_indexes ──────
        if let QueryResult::Rows { rows, .. } = self.execute(
            "SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' ORDER BY indexname",
        )? {
            for r in rows {
                let def = crate::eval::value_to_text(&r.values[0]);
                // Constraint-backing indexes are recreated by the
                // table's own PRIMARY KEY / UNIQUE clauses.
                if def.contains("_pkey") || def.contains("_key\"") || def.contains("_key ") {
                    continue;
                }
                out.push_str(&format!("{def};\n"));
            }
        }

        // ── views, from their stored deterministic bodies ───────────
        let mut views: Vec<(String, Vec<String>, String)> = Vec::new();
        for (name, v) in self.active_catalog().views_all() {
            if name.starts_with("__spg_") {
                continue;
            }
            views.push((v.name.clone(), v.columns.clone(), v.body.clone()));
        }
        views.sort();
        for (name, columns, body) in views {
            let cols = if columns.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    columns
                        .iter()
                        .map(|c| quote_ident(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            out.push_str(&format!(
                "CREATE VIEW {}{cols} AS {body};\n",
                quote_ident(&name)
            ));
        }
        Ok(out)
    }
}

/// A column type as re-parseable DDL. `pg_data_type_text` is the
/// canonical name (information_schema's own renderer); the length /
/// precision parameters it reports separately are re-attached here,
/// because a dump that silently widens `varchar(9)` to `varchar`
/// changes what the restored table accepts. The bare `DataType`
/// Display was tried first and printed `NUMERIC(0)` for an
/// unconstrained NUMERIC — not SQL.
fn ddl_type(ty: spg_storage::DataType) -> String {
    use spg_storage::DataType as T;
    match ty {
        T::Varchar(n) if n > 0 => format!("varchar({n})"),
        T::Char(n) if n > 0 => format!("char({n})"),
        T::Numeric { precision, scale } if precision > 0 => {
            format!("numeric({precision},{scale})")
        }
        // information_schema reports every array as the single word
        // ARRAY (element in udt_name) — right for that catalog, not
        // SQL. Spell the element.
        T::TextArray => "text[]".into(),
        T::IntArray => "integer[]".into(),
        T::BigIntArray => "bigint[]".into(),
        T::SmallIntArray => "smallint[]".into(),
        T::FloatArray => "double precision[]".into(),
        T::BoolArray => "boolean[]".into(),
        T::NumericArray => "numeric[]".into(),
        T::DateArray => "date[]".into(),
        T::TimestampArray => "timestamp without time zone[]".into(),
        T::TimestamptzArray => "timestamp with time zone[]".into(),
        T::UuidArray => "uuid[]".into(),
        T::JsonArray => "json[]".into(),
        T::JsonbArray => "jsonb[]".into(),
        T::BytesArray => "bytea[]".into(),
        T::VarcharArray => "varchar[]".into(),
        T::CharArray => "char[]".into(),
        T::IntervalArray => "interval[]".into(),
        T::OidArray => "oid[]".into(),
        T::MoneyArray => "money[]".into(),
        other => crate::system_catalog::pg_data_type_text(other),
    }
}

/// Double-quote when the ident isn't a lowercase bare word.
fn quote_ident(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
    {
        s.into()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}
