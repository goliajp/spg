//! 9.0.0 — an index's definition, read back from what storage holds.
//!
//! Storage keeps an index as positions, expression texts and flags; the
//! statement that made it is gone. Two things need the statement again: a
//! copy (`CREATE TABLE … (LIKE … INCLUDING INDEXES)`), which must build the
//! same index on another table, and every reader that asks what the key
//! parts are. Both used to reconstruct a leading column and nothing else.

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{CreateIndexStatement, Expr, IndexColumnOrder, IndexMethod};
use spg_storage::{Index, IndexKind, TableSchema};

use crate::EngineError;

/// One key part of an index: a column, or an expression over the row.
pub(crate) enum KeyPart<'a> {
    Column(usize),
    Expression(&'a str),
}

/// The key parts of `idx`, leading part first.
pub(crate) fn key_parts(idx: &Index) -> Vec<KeyPart<'_>> {
    let n = 1 + idx.extra_column_positions.len();
    (0..n)
        .map(|i| match idx.part_expression(i) {
            Some(src) => KeyPart::Expression(src),
            None => KeyPart::Column(if i == 0 {
                idx.column_position
            } else {
                idx.extra_column_positions[i - 1]
            }),
        })
        .collect()
}

fn parse(idx: &Index, src: &str) -> Result<Expr, EngineError> {
    spg_sql::parser::parse_expression(src).map_err(|e| {
        EngineError::Unsupported(alloc::format!(
            "index {:?} expression {src:?} failed to re-parse: {e:?}",
            idx.name
        ))
    })
}

/// The `CREATE INDEX` that builds `idx` again, on `schema`'s columns.
///
/// `opclasses` is the per-part list the catalog records (leading part
/// first). The table name is left for the caller, which is copying it
/// somewhere else.
pub(crate) fn create_index_statement_of(
    idx: &Index,
    schema: &TableSchema,
    opclasses: Option<&[Option<String>]>,
) -> Result<CreateIndexStatement, EngineError> {
    let column_name = |pos: usize| -> Result<String, EngineError> {
        schema
            .columns
            .get(pos)
            .map(|c| c.name.clone())
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "index {:?} names column {pos}, which the table does not have",
                    idx.name
                ))
            })
    };
    let opclass_of = |i: usize| opclasses.and_then(|o| o.get(i)).cloned().flatten();
    let method = match idx.kind {
        IndexKind::BTree(_) | IndexKind::BTreeMulti(_) => IndexMethod::BTree,
        IndexKind::Nsw(_) => IndexMethod::Hnsw,
        IndexKind::Brin { .. } => IndexMethod::Brin,
        IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => IndexMethod::Gin,
    };
    let expression = idx
        .expression
        .as_deref()
        .map(|s| parse(idx, s))
        .transpose()?;
    let mut extra_columns = Vec::with_capacity(idx.extra_column_positions.len());
    let mut extra_expressions = Vec::with_capacity(idx.extra_column_positions.len());
    let mut extra_collations = Vec::with_capacity(idx.extra_column_positions.len());
    let mut extra_opclasses = Vec::with_capacity(idx.extra_column_positions.len());
    let mut extra_orders = Vec::with_capacity(idx.extra_column_positions.len());
    for (i, &pos) in idx.extra_column_positions.iter().enumerate() {
        extra_columns.push(column_name(pos)?);
        extra_expressions.push(
            idx.extra_expressions
                .get(i)
                .and_then(Option::as_deref)
                .map(|s| parse(idx, s))
                .transpose()?,
        );
        extra_collations.push(idx.extra_collations.get(i).cloned().flatten());
        extra_opclasses.push(opclass_of(i + 1));
        let o = idx.extra_orders.get(i).copied().unwrap_or_default();
        extra_orders.push(IndexColumnOrder {
            descending: o.descending,
            nulls_first: o.nulls_first,
        });
    }
    Ok(CreateIndexStatement {
        name: idx.name.clone(),
        concurrently: false,
        key_order: IndexColumnOrder {
            descending: idx.descending,
            nulls_first: idx.nulls_first,
        },
        key_collation: idx.collation.clone(),
        table: String::new(),
        // A storage index is built; only a partitioned parent's
        // declaration is `ONLY`.
        only: false,
        column: column_name(idx.column_position)?,
        nulls_not_distinct: idx.nulls_not_distinct,
        method,
        if_not_exists: false,
        included_columns: idx
            .included_columns
            .iter()
            .map(|&p| column_name(p))
            .collect::<Result<_, _>>()?,
        partial_predicate: idx
            .partial_predicate
            .as_deref()
            .map(|s| parse(idx, s))
            .transpose()?,
        expression,
        extra_columns,
        extra_orders,
        extra_expressions,
        extra_collations,
        extra_opclasses,
        is_unique: idx.is_unique,
        opclass: opclass_of(0),
        method_name: None,
    })
}
