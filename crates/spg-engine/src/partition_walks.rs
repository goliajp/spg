//! v7.37.16 (16.12) — catalog walks for partition introspection
//! functions (`pg_partition_root`, `pg_partition_ancestors`).
//!
//! Lives in its own module so the eval-layer builtins can call
//! it without depending on the engine-private `partition.rs`
//! helpers (which take an `&Catalog` but are scoped to engine
//! internals + tests).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_storage::{Catalog, PartitionRole};

/// Return the top-most partition ancestor of `name`. For a
/// non-partition table, returns `Some(name.to_string())` — callers
/// that need PG's `pg_partition_root` NULL-for-plain-tables rule
/// (round 770: PG answers NULL there, the old note here claimed
/// otherwise) must gate on `partition_role` themselves.
/// Returns `None` only when `name` isn't in the catalog at all.
#[must_use]
pub fn root_of(catalog: &Catalog, name: &str) -> Option<String> {
    let mut cur: String = name.to_string();
    let mut seen_depth: u32 = 0;
    // Bounded loop guards against catalog cycles (none should
    // exist — DDL gates reject them — but defensive).
    while seen_depth < 256 {
        let t = catalog.get(&cur)?;
        match &t.schema().partition_role {
            // Walk up via the partition's recorded parent_name.
            Some(PartitionRole::Range { parent_name, .. })
            | Some(PartitionRole::List { parent_name, .. })
            | Some(PartitionRole::Hash { parent_name, .. })
            | Some(PartitionRole::Default { parent_name }) => {
                cur = parent_name.clone();
            }
            // Parent of a partition tree, or a plain non-partition
            // table — either way, this is the root.
            _ => return Some(cur),
        }
        seen_depth += 1;
    }
    Some(cur)
}

/// Return the leaf → root chain of partition ancestors of
/// `name`. The first element is `name`; the last is the root.
/// Empty when `name` isn't in the catalog.
#[must_use]
pub fn ancestors_of(catalog: &Catalog, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur: String = name.to_string();
    let mut seen_depth: u32 = 0;
    while seen_depth < 256 {
        let Some(t) = catalog.get(&cur) else {
            return Vec::new();
        };
        out.push(cur.clone());
        match &t.schema().partition_role {
            Some(PartitionRole::Range { parent_name, .. })
            | Some(PartitionRole::List { parent_name, .. })
            | Some(PartitionRole::Hash { parent_name, .. })
            | Some(PartitionRole::Default { parent_name }) => {
                cur = parent_name.clone();
            }
            _ => return out,
        }
        seen_depth += 1;
    }
    out
}

/// v7.39 (read01 partitionfuncs.c) — the `pg_partition_tree(name)` row
/// set: `(relid, parentrelid, isleaf, level)`, BFS from `name` (levels
/// relative to it). `parentrelid` is the relation's REAL parent even
/// when the walk starts mid-tree (PG). A relation outside any
/// partition tree — or missing — yields no rows.
#[must_use]
pub fn tree_of(catalog: &Catalog, name: &str) -> Vec<(String, Option<String>, bool, i64)> {
    let Some(t) = catalog.get(name) else {
        return Vec::new();
    };
    if t.schema().partition_role.is_none() {
        return Vec::new();
    }
    let parent_of = |n: &str| -> Option<String> {
        match &catalog.get(n)?.schema().partition_role {
            Some(
                PartitionRole::Range { parent_name, .. }
                | PartitionRole::List { parent_name, .. }
                | PartitionRole::Hash { parent_name, .. }
                | PartitionRole::Default { parent_name },
            ) => Some(parent_name.clone()),
            _ => None,
        }
    };
    let is_leaf = |n: &str| -> bool {
        !matches!(
            catalog.get(n).map(|t| t.schema().partition_role.clone()),
            Some(Some(PartitionRole::Parent { .. }))
        )
    };
    let all = catalog.table_names();
    let mut out: Vec<(String, Option<String>, bool, i64)> =
        alloc::vec![(name.to_string(), parent_of(name), is_leaf(name), 0)];
    let mut frontier: Vec<String> = alloc::vec![name.to_string()];
    let mut level: i64 = 0;
    while !frontier.is_empty() && level < 256 {
        level += 1;
        let mut next: Vec<String> = Vec::new();
        for cand in &all {
            if let Some(p) = parent_of(cand) {
                if frontier.iter().any(|f| f == &p) {
                    out.push((cand.clone(), Some(p), is_leaf(cand), level));
                    next.push(cand.clone());
                }
            }
        }
        frontier = next;
    }
    out
}
