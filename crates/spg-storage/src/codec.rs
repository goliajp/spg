//! On-disk codec: catalog-snapshot (de)serialization free
//! functions, the dense row-body encoder/decoder, the low-level
//! `write_*` primitives, and the `Cursor` reader. Split out of
//! lib.rs (monster tier-3 cut 3). The `Catalog::serialize` /
//! `deserialize` methods stay in lib.rs and drive these through
//! crate-internal calls; the public dense-row surface
//! (`encode_row_body_dense` / `decode_row_body_dense` /
//! `row_body_encoded_len`) keeps its `spg_storage::*` paths via
//! crate-root re-exports.

use super::*;

/// Per-table deserialize body — schema, rows, indices. Pulled out of
/// `Catalog::deserialize` to keep the latter under the line-budget lint
/// and to give the row hot loop its own scope (so the borrow on `t`
/// stays scoped here rather than across the whole catalog loop).
pub(crate) fn deserialize_table(
    cur: &mut Cursor<'_>,
    cat: &mut Catalog,
    version: u8,
) -> Result<(), StorageError> {
    let table_name = cur.read_str()?;
    let name = table_name.clone();
    let col_count = cur.read_u16()? as usize;
    let mut cols = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let c_name = cur.read_str()?;
        let ty = cur.read_data_type()?;
        let nullable = cur.read_u8()? != 0;
        let default = match cur.read_u8()? {
            0 => None,
            1 => Some(cur.read_value()?),
            other => {
                return Err(StorageError::Corrupt(format!(
                    "unknown default tag: {other}"
                )));
            }
        };
        let auto_increment = cur.read_u8()? != 0;
        // Note: deserialiser sets runtime_default = None for
        // older catalogs (≤ v14). v15+ reads it from the
        // per-column appendix below.
        cols.push(ColumnSchema {
            name: c_name,
            ty,
            nullable,
            default,
            runtime_default: None,
            auto_increment,
            user_enum_type: None,
            user_composite_type: None,
            acl: alloc::vec::Vec::new(),
            user_domain_type: None,
            on_update_runtime: None,
            collation: Collation::Binary,
            is_unsigned: false,
            inline_enum_variants: None,
            inline_set_variants: None,
            generated_stored_expr: None,
            identity_always: false,
            default_text: None,
            auto_restart: None,
            scalar_row_source: false,
            mysql_int_width: None,
            mysql_fsp: None,
        });
    }
    let n_cols = cols.len();
    cat.create_table(TableSchema::new(name, cols))?;
    // Vec<Table> with insertion-order semantics — the just-pushed
    // table is at the end. Sidecar `by_name` is already wired up but
    // we skip the map lookup here since we know the position.
    let t = cat.tables.last_mut().expect("create_table just pushed");
    deserialize_rows(cur, t, n_cols)?;
    deserialize_indices(cur, t, version)?;
    // v6.7.2 — per-table hot_tier_bytes appendix. v11+ writes
    // `[u8 has_value][u64 LE value (if has_value)]`. v10 / v9 / v8
    // catalogs skip this entirely (the deserialiser reads no extra
    // bytes; the table's hot_tier_bytes stays None from
    // TableSchema::new).
    if version >= 11 {
        let has = cur.read_u8()?;
        let hot_tier_bytes = match has {
            0 => None,
            1 => Some(cur.read_u64()?),
            other => {
                return Err(StorageError::Corrupt(format!(
                    "hot_tier_bytes appendix: unknown has-value byte {other}"
                )));
            }
        };
        t.schema_mut().hot_tier_bytes = hot_tier_bytes;
    }
    // v7.6.1 — FOREIGN KEY appendix (FILE_VERSION 13+). v12 / v11 / …
    // catalogs skip this entirely.
    if version >= 13 {
        let fk_count = cur.read_u16()? as usize;
        let mut fks = Vec::with_capacity(fk_count);
        for _ in 0..fk_count {
            let name = match cur.read_u8()? {
                0 => None,
                1 => Some(cur.read_str()?),
                other => {
                    return Err(StorageError::Corrupt(format!(
                        "FK appendix: unknown has-name byte {other}"
                    )));
                }
            };
            let local_arity = cur.read_u16()? as usize;
            let mut local_columns = Vec::with_capacity(local_arity);
            for _ in 0..local_arity {
                local_columns.push(cur.read_u16()? as usize);
            }
            let parent_table = cur.read_str()?;
            let parent_arity = cur.read_u16()? as usize;
            if parent_arity != local_arity {
                return Err(StorageError::Corrupt(format!(
                    "FK arity mismatch in catalog: local {local_arity} vs parent {parent_arity}"
                )));
            }
            let mut parent_columns = Vec::with_capacity(parent_arity);
            for _ in 0..parent_arity {
                parent_columns.push(cur.read_u16()? as usize);
            }
            let on_delete = FkAction::from_tag(cur.read_u8()?).ok_or_else(|| {
                StorageError::Corrupt("FK appendix: unknown on_delete tag".into())
            })?;
            let on_update = FkAction::from_tag(cur.read_u8()?).ok_or_else(|| {
                StorageError::Corrupt("FK appendix: unknown on_update tag".into())
            })?;
            // v7.38 (read01, T29) — MATCH type appendix (FILE_VERSION 55+); older
            // catalogs default to Simple.
            let match_type = if version >= 55 {
                crate::MatchType::from_tag(cur.read_u8()?).ok_or_else(|| {
                    StorageError::Corrupt("FK appendix: unknown match_type tag".into())
                })?
            } else {
                crate::MatchType::Simple
            };
            // v7.39 (round 288) — constraint-timing byte (FILE_VERSION
            // 79+); older catalogs are NOT DEFERRABLE, which is what
            // they behaved as.
            let (deferrable, initially_deferred) = if version >= 79 {
                let bits = cur.read_u8()?;
                (bits & 1 != 0, bits & 2 != 0)
            } else {
                (false, false)
            };
            fks.push(ForeignKeyConstraint {
                name,
                local_columns,
                parent_table,
                parent_columns,
                on_delete,
                on_update,
                match_type,
                deferrable,
                initially_deferred,
            });
        }
        t.schema_mut().foreign_keys = fks;
    }
    // v7.9.19 — UniquenessConstraint appendix (FILE_VERSION 15+).
    // v14 and below skip this entirely.
    if version >= 15 {
        let uc_count = cur.read_u16()? as usize;
        let mut ucs = Vec::with_capacity(uc_count);
        for _ in 0..uc_count {
            let is_pk = cur.read_u8()? != 0;
            let arity = cur.read_u16()? as usize;
            let mut cols = Vec::with_capacity(arity);
            for _ in 0..arity {
                cols.push(cur.read_u16()? as usize);
            }
            // v7.13.0 — trailing `nulls_not_distinct` flag
            // (FILE_VERSION 23+). v22 and below skip — flag
            // defaults to false (= NULLS DISTINCT).
            let nulls_not_distinct = if version >= 23 {
                cur.read_u8()? != 0
            } else {
                false
            };
            ucs.push(UniquenessConstraint {
                is_primary_key: is_pk,
                columns: cols,
                nulls_not_distinct,
                // v7.39 (read01 round 48) — filled in by the v60
                // constraint-name appendix at the tail; < v60 stays None.
                name: None,
            });
        }
        t.schema_mut().uniqueness_constraints = ucs;
        // v7.9.21 — runtime_default appendix (FILE_VERSION 15+).
        let rt_count = cur.read_u16()? as usize;
        for _ in 0..rt_count {
            let pos = cur.read_u16()? as usize;
            let expr = cur.read_str()?;
            if let Some(col) = t.schema_mut().columns.get_mut(pos) {
                col.runtime_default = Some(expr);
            }
        }
    }
    // v7.13.0 — CHECK constraints appendix (FILE_VERSION 23+).
    // v22 and below leave the vec empty.
    if version >= 23 {
        let check_count = cur.read_u16()? as usize;
        let mut checks = Vec::with_capacity(check_count);
        for _ in 0..check_count {
            // v7.39 (read01 round 48) — the name rides the v60 appendix at
            // the tail; < v60 catalogs leave it None (pg_constraint then
            // synthesises PG's <table>_<col>_check form, as before).
            checks.push(crate::CheckConstraint {
                name: None,
                expr: cur.read_str()?,
            });
        }
        t.schema_mut().checks = checks;
    }
    // v7.17.0 Phase 1.4 — per-table user_enum_type appendix
    // (FILE_VERSION 29+). Layout: [u16 count] then
    // [u16 col_pos][str enum_name] per binding.
    if version >= 29 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            let ename = cur.read_str()?;
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.user_enum_type = Some(ename);
            }
        }
    }
    // v7.17.0 Phase 1.5 — per-table user_domain_type appendix
    // (FILE_VERSION 30+). Same shape as the enum one.
    if version >= 30 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            let dname = cur.read_str()?;
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.user_domain_type = Some(dname);
            }
        }
    }
    // v7.17.0 Phase 2.1 — per-table on_update_runtime appendix
    // (FILE_VERSION 32+). Sparse layout matches the enum/
    // domain bindings.
    if version >= 32 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            let expr_src = cur.read_str()?;
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.on_update_runtime = Some(expr_src);
            }
        }
    }
    // v7.17.0 Phase 2.5 — per-table collation appendix
    // (FILE_VERSION 34+). Sparse: only non-Binary columns
    // land. v33-and-below readers leave every column at its
    // ColumnSchema::new default (Binary). Unknown tags from a
    // forward-incompat snapshot read back as Binary.
    if version >= 34 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            let tag = cur.read_u8()?;
            let collation = match tag {
                Collation::TAG_CASE_INSENSITIVE => Collation::CaseInsensitive,
                _ => Collation::Binary,
            };
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.collation = collation;
            }
        }
    }
    // v7.17.0 Phase 4.4 — per-table is_unsigned appendix
    // (FILE_VERSION 35+). Sparse: only UNSIGNED columns land.
    // v34-and-below readers leave every column at
    // `is_unsigned = false`.
    if version >= 35 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.is_unsigned = true;
            }
        }
    }
    // v7.17.0 Phase 3.P0-36 — per-table inline_enum_variants
    // appendix (FILE_VERSION 41+). Sparse: only ENUM columns land.
    // v40-and-below readers leave every column at
    // `inline_enum_variants = None`.
    if version >= 41 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            let variant_count = cur.read_u16()? as usize;
            let mut variants = Vec::with_capacity(variant_count);
            for _ in 0..variant_count {
                variants.push(cur.read_str()?);
            }
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.inline_enum_variants = Some(variants);
            }
        }
    }
    // v7.17.0 Phase 3.P0-37 — per-table inline_set_variants
    // appendix (FILE_VERSION 42+). Sparse: only SET columns land.
    if version >= 42 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            let variant_count = cur.read_u16()? as usize;
            let mut variants = Vec::with_capacity(variant_count);
            for _ in 0..variant_count {
                variants.push(cur.read_str()?);
            }
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.inline_set_variants = Some(variants);
            }
        }
    }
    // v7.37.6-B — partition role appendix(FILE_VERSION 49+)。
    // v48 写入者从未到这里;v49+ 读出 Option<PartitionRole>。
    if version >= 49 {
        let role = read_partition_role(cur)?;
        t.schema_mut().partition_role = role;
    }
    // v7.37.7 — per-table generated_stored_expr appendix
    // (FILE_VERSION 50+). Sparse — populates any column whose entry
    // appears here; v49-and-below catalogs default every column to
    // generated_stored_expr = None.
    if version >= 50 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            let src = cur.read_str()?;
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.generated_stored_expr = Some(src);
            }
        }
    }
    // v7.38 (read01) — per-table default_text appendix (FILE_VERSION 58+).
    // Sparse; written right after the generated_stored_expr block, so it is
    // read here before the MVCC row appendix. v57-and-below catalogs default
    // every column to default_text = None.
    if version >= 58 {
        let default_count = cur.read_u16()? as usize;
        for _ in 0..default_count {
            let col_pos = cur.read_u16()? as usize;
            let src = cur.read_str()?;
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.default_text = Some(src);
            }
        }
    }
    // v7.39 (RLS) — per-table policy appendix + the two RLS flags
    // (FILE_VERSION 59+). Written after the default_text block and before the
    // MVCC appendix. v58-and-below catalogs default to no policies, RLS off.
    if version >= 59 {
        t.schema_mut().row_security = cur.read_u8()? != 0;
        t.schema_mut().force_row_security = cur.read_u8()? != 0;
        let policy_count = cur.read_u16()? as usize;
        let mut policies = alloc::vec::Vec::with_capacity(policy_count);
        for _ in 0..policy_count {
            let name = cur.read_str()?;
            let cmd = crate::PolicyCmd::from_wire_byte(cur.read_u8()?)
                .ok_or_else(|| StorageError::Corrupt("policy appendix: unknown cmd byte".into()))?;
            let permissive = cur.read_u8()? != 0;
            let role_count = cur.read_u16()? as usize;
            let mut roles = alloc::vec::Vec::with_capacity(role_count);
            for _ in 0..role_count {
                roles.push(cur.read_str()?);
            }
            let using_expr = if cur.read_u8()? != 0 {
                Some(cur.read_str()?)
            } else {
                None
            };
            let with_check_expr = if cur.read_u8()? != 0 {
                Some(cur.read_str()?)
            } else {
                None
            };
            policies.push(crate::PolicyDef {
                name,
                cmd,
                permissive,
                roles,
                using_expr,
                with_check_expr,
            });
        }
        t.schema_mut().policies = policies;
    }
    // v7.37.16 (Epic W) — per-row MVCC header + stable RowId appendix
    // (FILE_VERSION 53+). Overwrites the frozen headers + dense rowids
    // that `deserialize_rows` installed above, restoring xmin/xmax/flags
    // + real ids verbatim so a cross-checkpoint tombstone redo resolves
    // by RowId. v52-and-below catalogs skip this entirely (their reader
    // stops after the generated_stored_expr block); the frozen/dense
    // state left by `deserialize_rows` is the exact pre-v53 contract.
    if version >= 53 {
        read_mvcc_header_appendix(cur, t)?;
    }
    // v7.39 (read01 round 48) — constraint-name appendix (FILE_VERSION 60+),
    // index-aligned to the CHECK / uniqueness appendices decoded above. A
    // count mismatch means the appendix desynced from the schema — refuse
    // rather than mis-pair a name with the wrong constraint.
    if version >= 60 {
        let check_count = cur.read_u16()? as usize;
        if check_count != t.schema().checks.len() {
            return Err(StorageError::Corrupt(format!(
                "constraint-name appendix: {check_count} CHECK names != {} CHECK constraints                  for table {table_name:?}",
                t.schema().checks.len()
            )));
        }
        for i in 0..check_count {
            let name = if cur.read_u8()? != 0 {
                Some(cur.read_str()?)
            } else {
                None
            };
            t.schema_mut().checks[i].name = name;
        }
        let uc_count = cur.read_u16()? as usize;
        if uc_count != t.schema().uniqueness_constraints.len() {
            return Err(StorageError::Corrupt(format!(
                "constraint-name appendix: {uc_count} unique names != {} uniqueness constraints                  for table {table_name:?}",
                t.schema().uniqueness_constraints.len()
            )));
        }
        for i in 0..uc_count {
            let name = if cur.read_u8()? != 0 {
                Some(cur.read_str()?)
            } else {
                None
            };
            t.schema_mut().uniqueness_constraints[i].name = name;
        }
    }
    // v7.39 (read01 round 56) — per-table user_composite_type appendix
    // (FILE_VERSION 63+). Same sparse shape as the enum / domain ones: only
    // composite-typed columns land here. v62-and-below leave every column None,
    // so their composite columns stay plain JSON (the pre-epic behaviour).
    if version >= 63 {
        let binding_count = cur.read_u16()? as usize;
        for _ in 0..binding_count {
            let col_pos = cur.read_u16()? as usize;
            let cname = cur.read_str()?;
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.user_composite_type = Some(cname);
            }
        }
    }
    // v7.39 (read01 round 57) — owner + ACL (FILE_VERSION 64+).
    if version >= 64 {
        if cur.read_u8()? == 1 {
            let owner = cur.read_str()?;
            t.schema_mut().owner = Some(owner);
        }
        let acl_count = cur.read_u16()? as usize;
        let mut acl = alloc::vec::Vec::with_capacity(acl_count);
        for _ in 0..acl_count {
            let grantee = cur.read_str()?;
            let privs = cur.read_u16()?;
            let grantable = cur.read_u16()?;
            let grantor = cur.read_str()?;
            acl.push(crate::AclItem {
                grantee,
                privs,
                grantable,
                grantor,
            });
        }
        t.schema_mut().acl = acl;
    }
    // v7.39 (read01 round 59) — column-level ACL (FILE_VERSION 65+).
    if version >= 65 {
        let ncols = cur.read_u16()? as usize;
        for _ in 0..ncols {
            let col_pos = cur.read_u16()? as usize;
            let n = cur.read_u16()? as usize;
            let mut acl = alloc::vec::Vec::with_capacity(n);
            for _ in 0..n {
                let grantee = cur.read_str()?;
                let privs = cur.read_u16()?;
                let grantable = cur.read_u16()?;
                let grantor = cur.read_str()?;
                acl.push(crate::AclItem {
                    grantee,
                    privs,
                    grantable,
                    grantor,
                });
            }
            if let Some(col) = t.schema_mut().columns.get_mut(col_pos) {
                col.acl = acl;
            }
        }
    }
    // v7.39 (round 210) — EXCLUDE-constraint appendix (FILE_VERSION 72+).
    if version >= 72 {
        let n = cur.read_u16()? as usize;
        let mut excls = alloc::vec::Vec::with_capacity(n);
        for _ in 0..n {
            let name = cur.read_str()?;
            let method = if cur.read_u8()? == 1 {
                Some(cur.read_str()?)
            } else {
                None
            };
            let elem_count = cur.read_u16()? as usize;
            let mut elements = alloc::vec::Vec::with_capacity(elem_count);
            for _ in 0..elem_count {
                let pos = cur.read_u16()? as usize;
                let op = cur.read_str()?;
                elements.push((pos, op));
            }
            excls.push(crate::ExclusionConstraint {
                name,
                method,
                elements,
            });
        }
        t.schema_mut().exclusion_constraints = excls;
    }
    // v7.39 (round 220) — identity-RESTART appendix (FILE_VERSION 73+).
    if version >= 73 {
        let n = cur.read_u16()? as usize;
        for _ in 0..n {
            let pos = cur.read_u16()? as usize;
            let floor = cur.read_i64()?;
            if let Some(col) = t.schema_mut().columns.get_mut(pos) {
                col.auto_restart = Some(floor);
            }
        }
    }
    // v7.39 (round 386, type-fidelity epic P1) — mysql_int_width appendix
    // (FILE_VERSION 81+). Sparse: only TINYINT / MEDIUMINT columns. v80-and-
    // below catalogs leave every column at None.
    if version >= 81 {
        let n = cur.read_u16()? as usize;
        for _ in 0..n {
            let pos = cur.read_u16()? as usize;
            let tag = cur.read_u8()?;
            let width = match tag {
                0 => MysqlIntWidth::Tiny,
                1 => MysqlIntWidth::Medium,
                2 => MysqlIntWidth::Small,
                3 => MysqlIntWidth::Int,
                // v7.39 (round 471, epic P4b) — BIGINT UNSIGNED.
                4 => MysqlIntWidth::Big,
                other => {
                    return Err(StorageError::Corrupt(format!(
                        "unknown mysql_int_width tag {other}"
                    )));
                }
            };
            if let Some(col) = t.schema_mut().columns.get_mut(pos) {
                col.mysql_int_width = Some(width);
            }
        }
    }
    // v7.39 (round 424, type-fidelity epic) — mysql_fsp appendix
    // (FILE_VERSION 82+). Sparse: only MySQL-declared temporal columns.
    // v81-and-below catalogs leave every column at None.
    if version >= 82 {
        let n = cur.read_u16()? as usize;
        for _ in 0..n {
            let pos = cur.read_u16()? as usize;
            let fsp = cur.read_u8()?;
            if fsp > 6 {
                return Err(StorageError::Corrupt(format!(
                    "mysql_fsp out of range: {fsp}"
                )));
            }
            if let Some(col) = t.schema_mut().columns.get_mut(pos) {
                col.mysql_fsp = Some(fsp);
            }
        }
    }
    let _ = table_name;
    Ok(())
}

/// v7.37.16 (Epic W) — parse the FILE_VERSION 53+ per-row MVCC header +
/// stable RowId appendix and reconstruct `Table::headers` / `rowids` /
/// `next_rowid` verbatim (replacing the frozen/dense placeholders
/// `deserialize_rows` installed). See the FILE_VERSION 53 docstring for
/// the on-disk layout. Every field read goes through `Cursor` helpers,
/// which return `StorageError::Corrupt` on a short/truncated image — no
/// panic on untrusted bytes.
fn read_mvcc_header_appendix(cur: &mut Cursor<'_>, t: &mut Table) -> Result<(), StorageError> {
    let count = cur.read_u32()? as usize;
    // Cross-check against the rows block already decoded. A mismatch means
    // the appendix desynced from the row stream (corruption) — refuse
    // rather than silently mis-pair headers with rows.
    if count != t.rows.len() {
        return Err(StorageError::Corrupt(format!(
            "MVCC header appendix row count {count} != decoded rows {} \
             for table {:?}",
            t.rows.len(),
            t.schema.name
        )));
    }
    let mut headers: PersistentVec<crate::row_header::RowHeader> = PersistentVec::new();
    let mut rowids: PersistentVec<crate::row_header::RowId> = PersistentVec::new();
    let mut max_id: u64 = 0;
    // v7.37.16 (autovacuum) — recount dead rows while restoring;
    // the incremental counter is not persisted.
    let mut dead: u64 = 0;
    for _ in 0..count {
        let xmin = cur.read_u64()?;
        let xmax = cur.read_u64()?;
        let flags = cur.read_u8()?;
        let rowid = cur.read_u64()?;
        if xmax != crate::row_header::XMAX_ALIVE {
            dead += 1;
        }
        // v7.38 — recover the process-global version cursor past every
        // persisted version, exactly as `next_rowid` is recovered past every
        // persisted RowId below. Without this a fresh process takes snapshots
        // at a version below the restored rows' `xmin`, so `Snapshot::visible`
        // reads them as "written by a future transaction" and every row a
        // previous process committed after the first one silently disappears.
        crate::row_header::observe_persisted_version(xmin);
        crate::row_header::observe_persisted_version(xmax);
        headers.push_mut(crate::row_header::RowHeader { xmin, xmax, flags });
        rowids.push_mut(crate::row_header::RowId(rowid));
        if rowid > max_id {
            max_id = rowid;
        }
    }
    let persisted_next_rowid = cur.read_u64()?;
    t.headers = headers;
    t.rowids = rowids;
    t.set_dead_rows_on_load(dead);
    // Keep `next_rowid` strictly above every loaded id so a future alloc
    // can never collide with a restored row. Trust the persisted cursor,
    // but clamp up defensively: a corrupt image that under-states it must
    // not hand out a colliding id.
    t.next_rowid = persisted_next_rowid.max(max_id + 1);
    debug_assert_eq!(
        t.rows.len(),
        t.headers.len(),
        "headers must be lock-step with rows after MVCC appendix restore"
    );
    debug_assert_eq!(
        t.rows.len(),
        t.rowids.len(),
        "rowids must be lock-step with rows after MVCC appendix restore"
    );
    Ok(())
}

fn deserialize_rows(
    cur: &mut Cursor<'_>,
    t: &mut Table,
    _n_cols: usize,
) -> Result<(), StorageError> {
    let row_count = cur.read_u32()? as usize;
    // v4.39: PV has no `reserve` (the BVT doesn't preallocate a
    // contiguous buffer); we just push directly and let the trie
    // grow. v5.1: row decode reuses `decode_row_body_dense` so the
    // catalog and cold-tier segments share one row codec.
    let mut hot_bytes: u64 = 0;
    for _ in 0..row_count {
        let tail = &cur.buf[cur.pos..];
        let (row, consumed) = decode_row_body_dense(tail, &t.schema, cur.codec_version)?;
        cur.pos += consumed;
        // v5.2.1: account for hot bytes as we go; the snapshot's row
        // block bytes are exactly what `encode_row_body_dense` would
        // produce, so `consumed` would do too — but going via the
        // helper keeps the counter's definition coupled to the
        // encoder rather than the snapshot's row prefix layout.
        hot_bytes = hot_bytes.saturating_add(row_body_encoded_len(&row, &t.schema) as u64);
        t.rows.push_mut(row);
        // v7.37.15 (Phase A.3) — keep headers lock-step with rows on
        // snapshot restore. Pre-MVCC snapshots and current snapshots
        // both load rows as RowHeader::frozen so every visibility
        // check against any snapshot returns true. v7.37.15 Phase E
        // will add a parallel `headers` stream in the snapshot codec
        // to round-trip actual xmin / xmax per row; for now the
        // load path mirrors the insert path's lock-step invariant.
        t.headers.push_mut(crate::row_header::RowHeader::frozen());
    }
    debug_assert_eq!(
        t.rows.len(),
        t.headers.len(),
        "headers must stay in lock-step with rows after snapshot restore"
    );
    t.hot_bytes = hot_bytes;
    // v7.37.15 (Phase C.1) — assign fresh dense stable ids to the
    // restored rows so `rowids` joins the lock-step invariant. Pre-V6
    // envelopes carry no ids; a dense 1..=len assignment is correct
    // while ids are process-local bookkeeping. The V6 envelope (Phase
    // C.6) will round-trip real ids so a WAL redo can name a row
    // across restart.
    t.assign_dense_rowids();
    Ok(())
}

fn deserialize_indices(
    cur: &mut Cursor<'_>,
    t: &mut Table,
    version: u8,
) -> Result<(), StorageError> {
    let index_count = cur.read_u16()? as usize;
    for _ in 0..index_count {
        let idx_name = cur.read_str()?;
        let col_pos = cur.read_u16()? as usize;
        let column_name = t
            .schema
            .columns
            .get(col_pos)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "index {idx_name:?} points at non-existent column position {col_pos}"
                ))
            })?
            .name
            .clone();
        let kind_tag = cur.read_u8()?;
        match kind_tag {
            0 => {
                if version >= 9 {
                    // v9+: BTree entries serialised inline (tag-prefixed
                    // locator codec). Restore the map directly so any
                    // freezer-produced Cold locators come back exactly
                    // as they went out.
                    let map = read_btree_map(cur)?;
                    t.restore_btree_index(idx_name, &column_name, map)?;
                } else {
                    // v8: no entries on disk; rebuild from rows. Every
                    // entry is materialised as `RowLocator::Hot(i)` —
                    // semantically identical to the v5.1 in-memory state
                    // since v8 catalogs never produced Cold locators.
                    t.add_index(idx_name, &column_name)?;
                }
            }
            1 => {
                let m = cur.read_u16()? as usize;
                let graph = cur.read_nsw_graph(m)?;
                t.restore_nsw_index(idx_name, &column_name, graph)?;
            }
            2 => {
                // v6.7.1 — BRIN tag. Payload is the column type
                // tag. No further data — summaries live in cold
                // segments.
                let column_type = cur.read_data_type()?;
                t.restore_brin_index(idx_name, &column_name, column_type)?;
            }
            3 => {
                // v7.12.3 — GIN tag. Payload mirrors the BTree
                // encoding but with String (lexeme word) keys.
                // Only emitted by FILE_VERSION 21+ writers — v20
                // and earlier degraded `USING gin` to BTree.
                let map = read_gin_map(cur)?;
                t.restore_gin_index(idx_name, &column_name, map)?;
            }
            4 => {
                // v7.15.0 — trigram-GIN tag (`gin_trgm_ops`).
                // Same payload shape as tag 3 (String → posting
                // list); only emitted by FILE_VERSION 24+ writers.
                if version < 24 {
                    return Err(StorageError::Corrupt(format!(
                        "trigram-GIN index tag 4 found in catalog FILE_VERSION {version}; \
                         FILE_VERSION 24+ required (v7.15.0 introduced this tag)"
                    )));
                }
                let map = read_gin_map(cur)?;
                t.restore_gin_trgm_index(idx_name, &column_name, map)?;
            }
            5 => {
                // v7.17.0 Phase 2.2 — fulltext-GIN tag (MySQL
                // `FULLTEXT KEY` surface). Same payload shape as
                // tag 3 / tag 4 (String → posting list); only
                // emitted by FILE_VERSION 33+ writers.
                if version < 33 {
                    return Err(StorageError::Corrupt(format!(
                        "fulltext-GIN index tag 5 found in catalog FILE_VERSION {version}; \
                         FILE_VERSION 33+ required (v7.17.0 Phase 2.2 introduced this tag)"
                    )));
                }
                let map = read_gin_map(cur)?;
                t.restore_gin_fulltext_index(idx_name, &column_name, map)?;
            }
            6 => {
                // v7.37.8(sentori Epic 5 P2)— JSONB-GIN tag.
                // Same payload shape as tags 3/4/5(String →
                // posting list); only emitted by FILE_VERSION 51+
                // writers. Pre-7.37.8 the same DDL loaded as a
                // BTree fallback so v50 catalogs never wrote a 6.
                if version < 51 {
                    return Err(StorageError::Corrupt(format!(
                        "JSONB-GIN index tag 6 found in catalog FILE_VERSION {version}; \
                         FILE_VERSION 51+ required (v7.37.8 introduced this tag)"
                    )));
                }
                let map = read_gin_map(cur)?;
                t.restore_gin_jsonb_index(idx_name, &column_name, map)?;
            }
            other => {
                return Err(StorageError::Corrupt(format!(
                    "unknown index kind tag: {other}"
                )));
            }
        }
        // v6.8.0 — included_columns appendix per index. v11- snapshots
        // stop before this u16; v12+ always carries it (possibly 0).
        if version >= 12 {
            let num_included = cur.read_u16()? as usize;
            if num_included > 0 {
                let mut included: Vec<usize> = Vec::with_capacity(num_included);
                for _ in 0..num_included {
                    let cp = cur.read_u16()? as usize;
                    if cp >= t.schema.columns.len() {
                        return Err(StorageError::Corrupt(format!(
                            "INCLUDE column position {cp} out of range \
                             ({} schema columns)",
                            t.schema.columns.len()
                        )));
                    }
                    included.push(cp);
                }
                if let Some(last) = t.indices.last_mut() {
                    last.included_columns = included;
                }
            }
            // v6.8.1 — partial_predicate appendix.
            match cur.read_u8()? {
                0 => {}
                1 => {
                    let pred = cur.read_str()?;
                    if let Some(last) = t.indices.last_mut() {
                        last.partial_predicate = Some(pred);
                    }
                }
                other => {
                    return Err(StorageError::Corrupt(format!(
                        "partial_predicate tag: unknown byte {other}"
                    )));
                }
            }
            // v6.8.2 — expression appendix.
            match cur.read_u8()? {
                0 => {}
                1 => {
                    let expr = cur.read_str()?;
                    if let Some(last) = t.indices.last_mut() {
                        last.expression = Some(expr);
                    }
                }
                other => {
                    return Err(StorageError::Corrupt(format!(
                        "expression tag: unknown byte {other}"
                    )));
                }
            }
            // v7.9.29 — is_unique appendix (FILE_VERSION 16+).
            // v15-and-below catalogs stop before this byte. mailrs K1.
            if version >= 16 {
                match cur.read_u8()? {
                    0 => {}
                    1 => {
                        if let Some(last) = t.indices.last_mut() {
                            last.is_unique = true;
                        }
                    }
                    other => {
                        return Err(StorageError::Corrupt(format!(
                            "is_unique tag: unknown byte {other}"
                        )));
                    }
                }
                // v7.9.29 — extra_column_positions appendix.
                let n = cur.read_u16()? as usize;
                if n > 0 {
                    let mut extras: Vec<usize> = Vec::with_capacity(n);
                    for _ in 0..n {
                        let cp = cur.read_u16()? as usize;
                        if cp >= t.schema.columns.len() {
                            return Err(StorageError::Corrupt(format!(
                                "extra column position {cp} out of range \
                                 ({} schema columns)",
                                t.schema.columns.len()
                            )));
                        }
                        extras.push(cp);
                    }
                    if let Some(last) = t.indices.last_mut() {
                        last.extra_column_positions = extras;
                    }
                }
                // v7.39 (read01 round 52) — nulls_not_distinct (FILE_VERSION
                // 62+). v61-and-below leave the flag false (NULLS DISTINCT).
                if version >= 62 {
                    let nnd = cur.read_u8()? != 0;
                    if let Some(last) = t.indices.last_mut() {
                        last.nulls_not_distinct = nnd;
                    }
                }
                // v7.39 (round 537) — the key column's ordering clause
                // (FILE_VERSION 83+). v82-and-below leave it ascending
                // with PG's default nulls placement, which is what those
                // snapshots recorded.
                if version >= 83 {
                    let desc = cur.read_u8()? != 0;
                    let nulls = match cur.read_u8()? {
                        0 => None,
                        1 => Some(true),
                        2 => Some(false),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "index nulls-order tag: unknown byte {other}"
                            )));
                        }
                    };
                    if let Some(last) = t.indices.last_mut() {
                        last.descending = desc;
                        last.nulls_first = nulls;
                    }
                }
                // v7.39 (round 538) — the key's explicit collation
                // (FILE_VERSION 84+).
                if version >= 84 {
                    let coll = match cur.read_u8()? {
                        0 => None,
                        1 => Some(cur.read_str()?),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "index collation tag: unknown byte {other}"
                            )));
                        }
                    };
                    if let Some(last) = t.indices.last_mut() {
                        last.collation = coll;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Parse a v9 `BTree` index payload — `[u32 entry_count]` followed by
/// `entry_count` `(IndexKey, Vec<RowLocator>)` pairs. The locator list
/// uses the v5.1 tag-prefixed wire format (`RowLocator::read_le`).
fn read_btree_map(
    cur: &mut Cursor<'_>,
) -> Result<PersistentBTreeMap<IndexKey, Vec<RowLocator>>, StorageError> {
    let entry_count = cur.read_u32()? as usize;
    let mut map = PersistentBTreeMap::new();
    for _ in 0..entry_count {
        let key = cur.read_index_key()?;
        let locator_count = cur.read_u32()? as usize;
        let mut locators = Vec::with_capacity(locator_count);
        for _ in 0..locator_count {
            let tail = &cur.buf[cur.pos..];
            let (loc, consumed) = RowLocator::read_le(tail).map_err(|e| {
                StorageError::Corrupt(format!("row_locator decode at offset {}: {e}", cur.pos))
            })?;
            cur.pos += consumed;
            locators.push(loc);
        }
        map.insert_mut(key, locators);
    }
    Ok(map)
}

/// v7.12.3 — parse a `Gin` index payload. Mirrors [`read_btree_map`]
/// but with `String` (lexeme word) keys instead of `IndexKey`.
/// FILE_VERSION 21+ only.
fn read_gin_map(
    cur: &mut Cursor<'_>,
) -> Result<PersistentBTreeMap<String, Vec<RowLocator>>, StorageError> {
    let entry_count = cur.read_u32()? as usize;
    let mut map = PersistentBTreeMap::new();
    for _ in 0..entry_count {
        let word = cur.read_str()?;
        let locator_count = cur.read_u32()? as usize;
        let mut locators = Vec::with_capacity(locator_count);
        for _ in 0..locator_count {
            let tail = &cur.buf[cur.pos..];
            let (loc, consumed) = RowLocator::read_le(tail).map_err(|e| {
                StorageError::Corrupt(format!("row_locator decode at offset {}: {e}", cur.pos))
            })?;
            cur.pos += consumed;
            locators.push(loc);
        }
        map.insert_mut(word, locators);
    }
    Ok(map)
}

// --- low-level binary helpers ---------------------------------------------

/// Write a `DataType` as a tag byte + optional payload (Vector carries its
/// `u32` dimension). Inverse: [`read_data_type`].
/// Serialize an HNSW graph after the `[kind=1][u16 M]` header (v7).
/// Layout:
/// - `[u16 m_max_0]`
/// - `[entry u32]` — `u32::MAX` means `None`, else the entry node index
/// - `[u8 entry_level]`
/// - `[node_count u32]`
/// - for each node: `[u8 level]`  (top layer for this node)
/// - `[layer_count u8]`
/// - for each layer `0..layer_count`:
///     - `[u32 layer_node_count]` (== `node_count`; per-layer slot)
///     - for each node: `[u16 neighbor_count] [u32 neighbor]*`
pub(crate) fn write_nsw_graph(out: &mut Vec<u8>, g: &NswGraph) {
    let entry = g.entry.map_or(u32::MAX, |e| {
        u32::try_from(e).expect("NSW entry fits in u32")
    });
    write_u16(
        out,
        u16::try_from(g.m_max_0).expect("HNSW m_max_0 fits in u16"),
    );
    out.extend_from_slice(&entry.to_le_bytes());
    out.push(g.entry_level);
    let node_count = g.levels.len();
    write_u32(
        out,
        u32::try_from(node_count).expect("HNSW node count fits in u32"),
    );
    for &lvl in &g.levels {
        out.push(lvl);
    }
    let layer_count = u8::try_from(g.layers.len()).expect("HNSW layer count ≤ 255");
    out.push(layer_count);
    for layer in &g.layers {
        write_u32(
            out,
            u32::try_from(layer.len()).expect("HNSW per-layer node count fits in u32"),
        );
        for neighbors in layer {
            write_u16(
                out,
                u16::try_from(neighbors.len()).expect("HNSW neighbour list fits in u16"),
            );
            // v6.1.x: neighbour slot is already u32 in memory; just
            // emit the raw bytes. (v6.0 stored usize and converted
            // here.)
            for &peer in neighbors {
                write_u32(out, peer);
            }
        }
    }
}

pub(crate) fn write_data_type(out: &mut Vec<u8>, t: DataType) {
    match t {
        DataType::Int => out.push(1),
        DataType::BigInt => out.push(2),
        DataType::Float => out.push(3),
        // v7.39 (round 271) — tag 68. Real used to write 66, which
        // PgLsn also writes and which the reader maps to PgLsn, so a
        // persisted REAL column came back as pg_lsn. The collision was
        // unreachable until round 269 made REAL a column type.
        DataType::Real => out.push(68),
        DataType::Text => out.push(4),
        // v7.39 (round 291) — tag 72. `name` is stored exactly like
        // TEXT; only the type identity is new, so no existing byte
        // moves and no old image needs migrating.
        DataType::Name => out.push(72),
        // v7.39 (round 640) — tags 73 / 74. Both store the i64 a BIGINT
        // stores; the tag is what tells a reader which identity the
        // column was declared with.
        DataType::Xid => out.push(73),
        DataType::Xid8 => out.push(74),
        DataType::Bool => out.push(5),
        DataType::Vector { dim, encoding } => match encoding {
            // Tag 6: pre-v6 F32 vector. Layout unchanged; pre-v6
            // binaries continue to deserialise this exactly as
            // before.
            VecEncoding::F32 => {
                out.push(6);
                out.extend_from_slice(&dim.to_le_bytes());
            }
            // v6.0.3: tag 15 for `VECTOR(N) USING HALF`. Same
            // forward-compat fence story as SQ8 below.
            VecEncoding::F16 => {
                out.push(15);
                out.extend_from_slice(&dim.to_le_bytes());
            }
            // v6.0.1: new tag 14 for `VECTOR(N) USING SQ8` column
            // type. Pre-v6 readers fall through `read_data_type`'s
            // catch-all and surface `Corrupt("unknown data type tag")`
            // — the explicit forward-compat fence called out in
            // V6_DESIGN deliberation #5.
            VecEncoding::Sq8 => {
                out.push(14);
                out.extend_from_slice(&dim.to_le_bytes());
            }
        },
        DataType::SmallInt => out.push(7),
        DataType::Varchar(max) => {
            out.push(8);
            out.extend_from_slice(&max.to_le_bytes());
        }
        DataType::Char(size) => {
            out.push(9);
            out.extend_from_slice(&size.to_le_bytes());
        }
        DataType::Numeric { precision, scale } => {
            // v7.39 (round 271) — tag 10 keeps its one-byte scale so
            // every catalog already on disk reads back unchanged; tag 69
            // carries the u16 scale a value can now have.
            // v7.39 (round 272) — precision joined scale in widening, so
            // tag 10 (the shape every catalog on disk already uses) now
            // needs BOTH to fit a byte; tag 69 carries the wide pair.
            if let (Ok(np), Ok(ns)) = (u8::try_from(precision), u8::try_from(scale)) {
                out.push(10);
                out.push(np);
                out.push(ns);
            } else {
                out.push(69);
                out.extend_from_slice(&precision.to_le_bytes());
                out.extend_from_slice(&scale.to_le_bytes());
            }
        }
        DataType::Date => out.push(11),
        DataType::Timestamp => out.push(12),
        // v7.9.2 — tag 17 for TIMESTAMPTZ. Body = i64 microseconds
        // UTC, identical to tag 12. Only the schema-side type tag
        // differs (for wire OID advertisement).
        DataType::Timestamptz => out.push(17),
        // v7.37.5 β-P2: tag 34 — INTERVAL. No body in the type slot;
        // the per-cell body is 16 bytes (i64 micros + i32 days +
        // i32 months) carried by `write_value_body`. Catalog
        // FILE_VERSION 48+.
        DataType::Interval => out.push(34),
        // v7.37.5 β-P4: tag 35 — INTERVAL[]. Catalog FILE_VERSION 48+.
        DataType::IntervalArray => out.push(35),
        // v7.37.5 γ — array-of-scalar family. Tags 36..48. Each
        // body shape follows `IntervalArray`: [u16 count][per
        // elem: u8 null + (non-null) scalar body in the codec's
        // LE form].
        DataType::BoolArray => out.push(36),
        DataType::SmallIntArray => out.push(37),
        DataType::FloatArray => out.push(38),
        DataType::NumericArray => out.push(39),
        DataType::DateArray => out.push(40),
        DataType::TimestampArray => out.push(41),
        DataType::TimestamptzArray => out.push(42),
        DataType::UuidArray => out.push(43),
        DataType::JsonArray => out.push(44),
        DataType::JsonbArray => out.push(45),
        DataType::BytesArray => out.push(46),
        DataType::VarcharArray => out.push(47),
        DataType::CharArray => out.push(48),
        // v7.37.5 δ: tag 49 + 1-byte RangeKind — multirange.
        // Catalog FILE_VERSION 48+.
        DataType::Multirange(k) => {
            out.push(49);
            out.push(k.tag());
        }
        // v7.37.5 ε: tags 50..56 — PG geometry scalar family.
        // No type-slot body; per-cell body lives in
        // write_value_body. Catalog FILE_VERSION 48+.
        DataType::Point => out.push(50),
        DataType::Lseg => out.push(51),
        DataType::Path => out.push(52),
        DataType::PgBox => out.push(53),
        DataType::Polygon => out.push(54),
        DataType::Line => out.push(55),
        DataType::Circle => out.push(56),
        // v7.37.5 ζ-A: tags 57..65 — network / bit / xml /
        // "char" / money[]. Catalog FILE_VERSION 48+.
        DataType::Inet => out.push(57),
        DataType::Cidr => out.push(58),
        DataType::Macaddr => out.push(59),
        DataType::Macaddr8 => out.push(60),
        // v7.39 (round 281) — tags 61/62 mean "no typmod", exactly what
        // every catalog already on disk holds; 70/71 carry the length a
        // typmod could not previously express.
        DataType::Bit(0) => out.push(61),
        DataType::BitVarying(0) => out.push(62),
        DataType::Bit(n) => {
            out.push(70);
            out.extend_from_slice(&n.to_le_bytes());
        }
        DataType::BitVarying(n) => {
            out.push(71);
            out.extend_from_slice(&n.to_le_bytes());
        }
        DataType::Xml => out.push(63),
        DataType::Char1 => out.push(64),
        DataType::MoneyArray => out.push(65),
        // v7.39 (read01 pg_lsn.c): tag 66. FILE_VERSION unchanged (additive).
        DataType::PgLsn => out.push(66),
        DataType::Json => out.push(13),
        // v7.9.0: tag 16 for `JSONB`. Same on-disk layout as
        // tag 13 — only the wire OID differs.
        DataType::Jsonb => out.push(16),
        // v7.10.4: tag 18 for `BYTEA`. Body = [u16 len][bytes].
        DataType::Bytes => out.push(18),
        // v7.10.9: tag 19 for `TEXT[]`. Body = [u16 count][per
        // element: u8 null + (if non-null) u16 len + utf-8].
        DataType::TextArray => out.push(19),
        // v7.11.12: tag 20 for `INT[]`. Body = [u16 count][per
        // element: u8 null + (if non-null) i32 LE].
        DataType::IntArray => out.push(20),
        // v7.11.12: tag 21 for `BIGINT[]`. Body = [u16 count][per
        // element: u8 null + (if non-null) i64 LE].
        DataType::BigIntArray => out.push(21),
        // v7.12.0: tag 22 for `tsvector`. No body — type identity
        // alone. Catalog FILE_VERSION 20+.
        DataType::TsVector => out.push(22),
        // v7.12.0: tag 23 for `tsquery`. No body. Catalog
        // FILE_VERSION 20+.
        DataType::TsQuery => out.push(23),
        // v7.17.0: tag 24 for `UUID`. No body — type identity
        // alone. Catalog FILE_VERSION 36+.
        DataType::Uuid => out.push(24),
        // v7.17.0 Phase 3.P0-32: tag 25 for `TIME`. No body — type
        // identity alone. Catalog FILE_VERSION 37+.
        DataType::Time => out.push(25),
        // v7.17.0 Phase 3.P0-33: tag 26 for `YEAR`. No body — type
        // identity alone. Catalog FILE_VERSION 38+.
        DataType::Year => out.push(26),
        // v7.17.0 Phase 3.P0-34: tag 27 for `TIMETZ`. No body —
        // type identity alone. Catalog FILE_VERSION 39+.
        DataType::TimeTz => out.push(27),
        // v7.17.0 Phase 3.P0-35: tag 28 for `MONEY`. No body —
        // type identity alone. Catalog FILE_VERSION 40+.
        DataType::Money => out.push(28),
        // v7.17.0 Phase 3.P0-38: tag 29 for range types. Body
        // = `[u8 RangeKind tag]`. Catalog FILE_VERSION 43+.
        DataType::Range(k) => {
            out.push(29);
            out.push(k.tag());
        }
        // v7.17.0 Phase 3.P0-39: tag 30 for hstore. No body —
        // type identity alone. Catalog FILE_VERSION 44+.
        DataType::Hstore => out.push(30),
        // v7.17.0 Phase 3.P0-40: tag 31/32/33 for 2D arrays.
        // No body — type identity alone. Catalog FILE_VERSION 45+.
        DataType::IntArray2D => out.push(31),
        DataType::BigIntArray2D => out.push(32),
        DataType::TextArray2D => out.push(33),
        // v7.39 (read01 round 75) — bool[][].
        DataType::BoolArray2D => out.push(67),
    }
}

impl Cursor<'_> {
    pub(crate) fn read_data_type(&mut self) -> Result<DataType, StorageError> {
        let tag = self.read_u8()?;
        match tag {
            1 => Ok(DataType::Int),
            2 => Ok(DataType::BigInt),
            3 => Ok(DataType::Float),
            4 => Ok(DataType::Text),
            72 => Ok(DataType::Name),
            73 => Ok(DataType::Xid),
            74 => Ok(DataType::Xid8),
            5 => Ok(DataType::Bool),
            6 => Ok(DataType::Vector {
                dim: self.read_u32()?,
                encoding: VecEncoding::F32,
            }),
            7 => Ok(DataType::SmallInt),
            8 => Ok(DataType::Varchar(self.read_u32()?)),
            9 => Ok(DataType::Char(self.read_u32()?)),
            10 => {
                let precision = u16::from(self.read_u8()?);
                let scale = i16::from(self.read_u8()?);
                Ok(DataType::Numeric { precision, scale })
            }
            69 => {
                let plo = self.read_u8()?;
                let phi = self.read_u8()?;
                let slo = self.read_u8()?;
                let shi = self.read_u8()?;
                Ok(DataType::Numeric {
                    precision: u16::from_le_bytes([plo, phi]),
                    // v7.39 (round 273) — the declared scale is signed;
                    // tag 69's two bytes carry it in two's complement.
                    scale: i16::from_le_bytes([slo, shi]),
                })
            }
            11 => Ok(DataType::Date),
            12 => Ok(DataType::Timestamp),
            13 => Ok(DataType::Json),
            14 => Ok(DataType::Vector {
                dim: self.read_u32()?,
                encoding: VecEncoding::Sq8,
            }),
            // v6.0.3: tag 15 for `VECTOR(N) USING HALF`. Same
            // [u32 dim] type-tag payload as F32 / SQ8; the encoding
            // lives in the tag byte itself.
            15 => Ok(DataType::Vector {
                dim: self.read_u32()?,
                encoding: VecEncoding::F16,
            }),
            // v7.9.0: tag 16 for `JSONB`. Storage shape == Json;
            // we only carry the type tag so the wire layer can
            // emit PG OID 3802 instead of 114.
            16 => Ok(DataType::Jsonb),
            // v7.9.2: tag 17 for `TIMESTAMPTZ`. Storage shape ==
            // Timestamp (i64 microseconds UTC); only the wire OID
            // (1184) differs.
            17 => Ok(DataType::Timestamptz),
            // v7.10.4: tag 18 for `BYTEA`. Catalog FILE_VERSION 17+.
            18 => Ok(DataType::Bytes),
            // v7.10.9: tag 19 for `TEXT[]`. Catalog FILE_VERSION 18+.
            19 => Ok(DataType::TextArray),
            // v7.11.12: tags 20/21 for INT[]/BIGINT[]. FILE_VERSION 19+.
            20 => Ok(DataType::IntArray),
            21 => Ok(DataType::BigIntArray),
            // v7.12.0: tags 22/23 for tsvector / tsquery. Catalog
            // FILE_VERSION 20+.
            22 => Ok(DataType::TsVector),
            23 => Ok(DataType::TsQuery),
            // v7.17.0: tag 24 — UUID. Catalog FILE_VERSION 36+.
            24 => Ok(DataType::Uuid),
            // v7.17.0 Phase 3.P0-32: tag 25 — TIME. Catalog
            // FILE_VERSION 37+.
            25 => Ok(DataType::Time),
            // v7.17.0 Phase 3.P0-33: tag 26 — YEAR. Catalog
            // FILE_VERSION 38+.
            26 => Ok(DataType::Year),
            // v7.17.0 Phase 3.P0-34: tag 27 — TIMETZ. Catalog
            // FILE_VERSION 39+.
            27 => Ok(DataType::TimeTz),
            // v7.17.0 Phase 3.P0-35: tag 28 — MONEY. Catalog
            // FILE_VERSION 40+.
            28 => Ok(DataType::Money),
            // v7.17.0 Phase 3.P0-38: tag 29 + RangeKind tag.
            29 => {
                let kt = self.read_u8()?;
                let k = RangeKind::from_tag(kt)
                    .ok_or_else(|| StorageError::Corrupt(format!("unknown RangeKind tag: {kt}")))?;
                Ok(DataType::Range(k))
            }
            // v7.17.0 Phase 3.P0-39: tag 30 — HSTORE.
            30 => Ok(DataType::Hstore),
            // v7.17.0 Phase 3.P0-40: tag 31/32/33 — 2D arrays.
            31 => Ok(DataType::IntArray2D),
            32 => Ok(DataType::BigIntArray2D),
            33 => Ok(DataType::TextArray2D),
            67 => Ok(DataType::BoolArray2D),
            // v7.37.5 β-P2: tag 34 — INTERVAL. Catalog FILE_VERSION 48+.
            34 => Ok(DataType::Interval),
            // v7.37.5 β-P4: tag 35 — INTERVAL[]. Catalog FILE_VERSION 48+.
            35 => Ok(DataType::IntervalArray),
            // v7.37.5 γ: tags 36..48 — array-of-scalar family.
            36 => Ok(DataType::BoolArray),
            37 => Ok(DataType::SmallIntArray),
            38 => Ok(DataType::FloatArray),
            39 => Ok(DataType::NumericArray),
            40 => Ok(DataType::DateArray),
            41 => Ok(DataType::TimestampArray),
            42 => Ok(DataType::TimestamptzArray),
            43 => Ok(DataType::UuidArray),
            44 => Ok(DataType::JsonArray),
            45 => Ok(DataType::JsonbArray),
            46 => Ok(DataType::BytesArray),
            47 => Ok(DataType::VarcharArray),
            48 => Ok(DataType::CharArray),
            // v7.37.5 δ: tag 49 + 1-byte RangeKind — multirange.
            49 => {
                let kt = self.read_u8()?;
                let k = RangeKind::from_tag(kt).ok_or_else(|| {
                    StorageError::Corrupt(format!("unknown RangeKind tag in multirange: {kt}"))
                })?;
                Ok(DataType::Multirange(k))
            }
            // v7.37.5 ε: tags 50..56 — PG geometry scalar family.
            50 => Ok(DataType::Point),
            51 => Ok(DataType::Lseg),
            52 => Ok(DataType::Path),
            53 => Ok(DataType::PgBox),
            54 => Ok(DataType::Polygon),
            55 => Ok(DataType::Line),
            56 => Ok(DataType::Circle),
            57 => Ok(DataType::Inet),
            58 => Ok(DataType::Cidr),
            59 => Ok(DataType::Macaddr),
            60 => Ok(DataType::Macaddr8),
            61 => Ok(DataType::Bit(0)),
            62 => Ok(DataType::BitVarying(0)),
            70 => Ok(DataType::Bit(self.read_u32()?)),
            71 => Ok(DataType::BitVarying(self.read_u32()?)),
            63 => Ok(DataType::Xml),
            64 => Ok(DataType::Char1),
            65 => Ok(DataType::MoneyArray),
            66 => Ok(DataType::PgLsn),
            68 => Ok(DataType::Real),
            other => Err(StorageError::Corrupt(format!(
                "unknown data type tag: {other}"
            ))),
        }
    }
}

/// Fast computation of the byte length [`encode_row_body_dense`]
/// would produce, without allocating the output buffer. Mirrors the
/// encoder's per-column body sizing so the v5.2.1 `Table::hot_bytes`
/// incremental counter doesn't pay an alloc-per-insert tax. Returns
/// the exact same `usize` as `encode_row_body_dense(row, schema).len()`.
pub fn row_body_encoded_len(row: &Row<'_>, schema: &TableSchema) -> usize {
    debug_assert_eq!(
        row.values.len(),
        schema.columns.len(),
        "row_body_encoded_len: row arity must match schema"
    );
    let bitmap_bytes = schema.columns.len().div_ceil(8);
    let mut n = bitmap_bytes;
    for (col_idx, v) in row.values.iter().enumerate() {
        if matches!(v, Value::Null) {
            continue;
        }
        n += value_body_encoded_len(v, schema.columns[col_idx].ty);
    }
    n
}

/// Byte length a single cell consumes when written by
/// `write_value_body`. Used by [`row_body_encoded_len`]; kept in
/// lock-step with the encoder. The `_ty` slot is reserved for future
/// type-dependent encodings — every variant currently writes a fixed
/// body shape regardless of the declared column type.
fn value_body_encoded_len(v: &Value<'_>, _ty: DataType) -> usize {
    match v {
        Value::SmallInt(_) => 2,
        // 4-byte body: i32 / Date.
        Value::Int(_) | Value::Date(_) => 4,
        // 8-byte body: i64 / f64 / Timestamp.
        Value::Real(_) => 4,
        Value::BigInt(_) | Value::Float(_) | Value::Timestamp(_) => 8,
        Value::Bool(_) => 1,
        // Text/Varchar/Char/Json share the [u16 len][utf-8] layout;
        // v7.23 — texts >= 64 KiB take the 6-byte escape header
        // (these sizes feed the freezer's hot-bytes budget, so the
        // estimate must not undercount).
        Value::Text(s) | Value::Json(s) => {
            if s.len() >= STR_LEN_ESCAPE as usize {
                6 + s.len()
            } else {
                2 + s.len()
            }
        }
        // [u32 dim][f32 * dim]
        Value::Vector(vec) => 4 + 4 * vec.len(),
        // v6.0.1: SQ8 cell on-disk shape — [u32 dim][f32 min]
        // [f32 max][u8 * dim] = 12 + dim bytes. `hot_bytes`
        // tracking on `Table::insert` calls this every row, so
        // returning the real size now (even though the actual
        // `write_value_body` writer lands in step 6) keeps the
        // sizing arithmetic honest for in-memory benches.
        Value::Sq8Vector(q) => 4 + 4 + 4 + q.bytes.len(),
        // v6.0.3: halfvec on-disk shape — [u32 dim][u16 LE * dim]
        // = 4 + 2 * dim bytes.
        Value::HalfVector(h) => 4 + h.bytes.len(),
        // [i128 scaled][u8 scale]
        Value::Numeric { .. } => 1 + 16 + 1, // form byte + scaled + scale (T6.P4)
        Value::NumericBig(b) => {
            let (_, l, _) = b.parts();
            1 + 1 + 2 + 4 * l.len()
        }
        // v7.10.4: BYTEA on-disk shape mirrors Text — [u16 len][bytes].
        // The 16-bit length cap is the same TEXT/JSON limit (~65 KB);
        // larger blobs need toast-style chunking which is a v7.11
        // carve-out (kept aligned with TEXT for now so the catalog
        // snapshot stays simple).
        Value::Bytes(b) => 2 + b.len(),
        // v7.10.9: TEXT[] on-disk shape — [u16 count][per element:
        // u8 null flag + (when non-null) u16 len + utf-8 bytes].
        Value::TextArray(items) => {
            let mut n = 2; // count prefix
            for item in items {
                n += 1; // null flag
                if let Some(s) = item {
                    n += 2 + s.len();
                }
            }
            n
        }
        // v7.11.12: INT[] / BIGINT[] — [u16 count][per element:
        // u8 null + (when non-null) fixed-width LE].
        Value::IntArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 5 } else { 1 })
                .sum::<usize>()
        }
        Value::BigIntArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 9 } else { 1 })
                .sum::<usize>()
        }
        // v7.37.5 β-P4: INTERVAL[] — [u16 count][per elem: u8 null +
        // (when non-null) 16-byte interval body].
        Value::IntervalArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 17 } else { 1 })
                .sum::<usize>()
        }
        // v7.37.5 γ — fixed-width per-element bodies.
        Value::BoolArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 2 } else { 1 })
                .sum::<usize>()
        }
        Value::SmallIntArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 3 } else { 1 })
                .sum::<usize>()
        }
        Value::FloatArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 9 } else { 1 })
                .sum::<usize>()
        }
        Value::TimestampArray(items) | Value::TimestamptzArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 9 } else { 1 })
                .sum::<usize>()
        }
        Value::DateArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 5 } else { 1 })
                .sum::<usize>()
        }
        Value::NumericArray(items) => {
            // i128 scaled (16) + u8 scale (1) + 1 null flag.
            2 + items
                .iter()
                .map(|x| if x.is_some() { 18 } else { 1 })
                .sum::<usize>()
        }
        Value::UuidArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 17 } else { 1 })
                .sum::<usize>()
        }
        // v7.37.5 γ — variable-width per-element bodies. The codec
        // uses v47 escape lengths for string/bytes bodies; size
        // includes the 1-byte null flag + escaped length header +
        // payload bytes. write_bytes_escaped/write_str_escaped_v47
        // emit [u16 0xFFFF][u32 real_len] when real_len >= u16::MAX.
        Value::JsonArray(items)
        | Value::JsonbArray(items)
        | Value::VarcharArray(items)
        | Value::CharArray(items) => {
            let mut n = 2;
            for it in items {
                n += 1; // null flag
                if let Some(s) = it {
                    n += if s.len() >= u16::MAX as usize {
                        2 + 4
                    } else {
                        2
                    } + s.len();
                }
            }
            n
        }
        Value::BytesArray(items) => {
            let mut n = 2;
            for it in items {
                n += 1;
                if let Some(b) = it {
                    n += if b.len() >= u16::MAX as usize {
                        2 + 4
                    } else {
                        2
                    } + b.len();
                }
            }
            n
        }
        // v7.37.5 δ — Multirange: [u16 count][per range: u8 flags
        // + (opt) lower body + (opt) upper body]. Bound bodies
        // recurse through `value_body_encoded_len` against the
        // element type of the range's kind. Rough estimate uses
        // 16 B per non-empty bound (covers i64/f64/i128 scalars);
        // the freezer's hot-bytes budget tolerates a moderate
        // overcount on this rarely-used type.
        Value::Multirange { ranges, .. } => {
            let mut n = 2;
            for r in ranges {
                n += 1; // flags
                if r.lower.is_some() {
                    n += 16;
                }
                if r.upper.is_some() {
                    n += 16;
                }
            }
            n
        }
        // v7.37.5 ε — geometry fixed-width bodies.
        Value::Point(_) => 16,                        // 2 × f64
        Value::Lseg(_, _) | Value::PgBox(_, _) => 32, // 2 × Point2D
        Value::Line { .. } => 24,                     // 3 × f64
        Value::Circle { .. } => 24,                   // Point + f64
        // v7.37.5 ε — Path: [u8 closed flag][u32 count][Point*n].
        Value::Path { points, .. } => 1 + 4 + 16 * points.len(),
        // v7.37.5 ε — Polygon: [u32 count][Point*n].
        Value::Polygon(points) => 4 + 16 * points.len(),
        // v7.37.5 ζ-A — network / bit / xml / "char" / money[].
        Value::Inet { .. } | Value::Cidr { .. } => 1 + 1 + 16, // family + bits + addr
        Value::Macaddr(_) => 6,
        Value::Macaddr8(_) => 8,
        Value::PgLsn(_) => 8,
        // BitString: [u32 nbits][packed bytes].
        Value::BitString { bytes, .. } => 4 + bytes.len(),
        Value::Xml(s) => {
            // Same envelope as TEXT (v47 escape lengths).
            if s.len() >= STR_LEN_ESCAPE as usize {
                6 + s.len()
            } else {
                2 + s.len()
            }
        }
        Value::BpChar(s) => {
            if s.len() >= STR_LEN_ESCAPE as usize {
                6 + s.len()
            } else {
                2 + s.len()
            }
        }
        Value::Char1(_) => 1,
        Value::MoneyArray(items) => {
            2 + items
                .iter()
                .map(|x| if x.is_some() { 9 } else { 1 })
                .sum::<usize>()
        }
        // v7.12.0: tsvector dense body — [u16 lexeme_count][per
        // lex: u16 word_len + utf-8 word + u16 pos_count + (u16
        // LE * pos_count) + u8 weight].
        Value::TsVector(lexs) => {
            let mut n = 2;
            for l in lexs {
                n += 2 + l.word.len() + 2 + 2 * l.positions.len() + 1;
            }
            n
        }
        // v7.12.0: tsquery dense body — prefix-coded tree.
        // Sizing must match `write_tsquery_body` walker.
        Value::TsQuery(ast) => tsquery_encoded_len(ast),
        // v7.17.0: UUID dense body — fixed 16 bytes, no prefix.
        Value::Uuid(_) => 16,
        // v7.17.0 Phase 3.P0-32: TIME dense body — fixed i64 LE.
        Value::Time(_) => 8,
        // v7.17.0 Phase 3.P0-33: YEAR dense body — fixed u16 LE.
        Value::Year(_) => 2,
        // v7.17.0 Phase 3.P0-34: TIMETZ dense body — i64 LE + i32 LE.
        Value::TimeTz { .. } => 12,
        // v7.17.0 Phase 3.P0-35: MONEY dense body — i64 LE cents.
        Value::Money(_) => 8,
        // v7.17.0 Phase 3.P0-38: range dense body — `[u8 flags]
        // [if lower: write_value(lower)] [if upper: write_value(upper)]`.
        // Element uses the schema-agnostic write_value codec
        // (which carries its own tag byte). The flags byte
        // captures empty/lower_some/upper_some/lower_inc/upper_inc.
        Value::Range { lower, upper, .. } => {
            1 + lower
                .as_ref()
                .map(|v| write_value_encoded_len(v))
                .unwrap_or(0)
                + upper
                    .as_ref()
                    .map(|v| write_value_encoded_len(v))
                    .unwrap_or(0)
        }
        // v7.17.0 Phase 3.P0-39: hstore dense body — `[u32 count]
        // then per pair [u32 klen][k bytes][u8 has_val][if has_val:
        // u32 vlen][v bytes]`.
        Value::Hstore(pairs) => {
            let mut n = 4;
            for (k, v) in pairs {
                n += 4 + k.len() + 1;
                if let Some(val) = v {
                    n += 4 + val.len();
                }
            }
            n
        }
        // v7.17.0 Phase 3.P0-40: 2D arrays dense body — `[u32 rows]
        // [u32 cols] then row-major elements with per-element
        // `[u8 null_flag][if non-null: element body]`.
        Value::IntArray2D(rows) => {
            let cols = rows.first().map(|r| r.len()).unwrap_or(0);
            8 + rows.len() * cols * (1 + 4)
        }
        Value::BigIntArray2D(rows) => {
            let cols = rows.first().map(|r| r.len()).unwrap_or(0);
            8 + rows.len() * cols * (1 + 8)
        }
        Value::BoolArray2D(rows) => {
            let cols = rows.first().map(|r| r.len()).unwrap_or(0);
            8 + rows.len() * cols
        }
        Value::TextArray2D(rows) => {
            let cols = rows.first().map(|r| r.len()).unwrap_or(0);
            let mut n = 8 + rows.len() * cols;
            for row in rows {
                for s in row.iter().flatten() {
                    n += 4 + s.len();
                }
            }
            n
        }
        // NULL is encoded only in the bitmap, never in the body.
        // v7.38 (read01, T9) — a composite/record is a transient value
        // (row() → to_json); it is never persisted, so it has no on-disk body.
        Value::Composite(_) => 0,
        // v7.39 (round 640) — an `xid` column persists the 8-byte body
        // its BIGINT sibling does.
        Value::Xid(_) => 8,
        Value::RegClass(..) | Value::RegProc(..) | Value::Tid(..) | Value::Cid(_) => 0,
        Value::Null => 0,
        // v7.37.5 β-P2 — INTERVAL is a 16-byte fixed body:
        // 8 i64 micros + 4 i32 days + 4 i32 months. PG byte-equal
        // layout (binary BIND/result uses the same field order).
        Value::Interval { .. } => 16,
    }
}

/// Encode one row's body in the v3.0.2 dense format (`FILE_VERSION`
/// 8): per-row NULL bitmap (1 bit/col, ceil(cols/8) bytes), then
/// each non-NULL cell as `write_value_body`. Same wire shape the
/// catalog snapshot writes per row inside its rows-block. Exposed
/// pub so v5.1+ cold-tier segment writers can produce row payloads
/// that the catalog [`decode_row_body_dense`] decodes 1:1.
///
/// `row.values.len()` must equal `schema.columns.len()` — the row
/// is expected to have been validated by `Table::insert` (the
/// engine's INSERT path) before reaching this function.
pub fn encode_row_body_dense(row: &Row<'_>, schema: &TableSchema) -> Vec<u8> {
    debug_assert_eq!(
        row.values.len(),
        schema.columns.len(),
        "dense encode: row arity must match schema"
    );
    let bitmap_bytes = schema.columns.len().div_ceil(8);
    // 8 B per fixed-width cell is a reasonable average; the buffer
    // grows past this for variable-width Text/Vector cells.
    let mut out = Vec::with_capacity(bitmap_bytes + schema.columns.len() * 8);
    let bitmap_offset = out.len();
    out.resize(bitmap_offset + bitmap_bytes, 0);
    for (i, v) in row.values.iter().enumerate() {
        if matches!(v, Value::Null) {
            out[bitmap_offset + i / 8] |= 1 << (i % 8);
        }
    }
    for (col_idx, v) in row.values.iter().enumerate() {
        if matches!(v, Value::Null) {
            continue;
        }
        write_value_body(&mut out, v, schema.columns[col_idx].ty);
    }
    out
}

/// Inverse of [`encode_row_body_dense`]. Reads one row's body from
/// `bytes` and returns it plus the number of bytes consumed (so a
/// caller decoding a back-to-back stream of rows can advance its
/// cursor). Returns `StorageError::Corrupt` on truncation, bad
/// UTF-8, or unknown cell tags.
pub fn decode_row_body_dense(
    bytes: &[u8],
    schema: &TableSchema,
    codec_version: u8,
) -> Result<(Row<'static>, usize), StorageError> {
    let mut cur = Cursor::new(bytes).with_codec_version(codec_version);
    let bitmap_bytes = schema.columns.len().div_ceil(8);
    let mut bitmap_buf = [0u8; 32];
    if bitmap_bytes > bitmap_buf.len() {
        return Err(StorageError::Corrupt(format!(
            "row NULL bitmap {bitmap_bytes} B exceeds 32 B cap"
        )));
    }
    let slice = cur.take(bitmap_bytes)?;
    bitmap_buf[..bitmap_bytes].copy_from_slice(slice);
    let mut values = Vec::with_capacity(schema.columns.len());
    for (col_idx, col) in schema.columns.iter().enumerate() {
        if (bitmap_buf[col_idx / 8] >> (col_idx % 8)) & 1 == 1 {
            values.push(Value::Null);
        } else {
            values.push(cur.read_value_body(col.ty)?);
        }
    }
    Ok((Row { values }, cur.pos))
}

/// Schema-driven dense value encoding (`FILE_VERSION` 8). Caller already
/// knows the column type and has decided this cell is non-NULL, so we
/// skip the per-cell type tag the v7 `write_value` was writing. NULL
/// is encoded via the per-row bitmap before this function runs, never
/// reaches here. Used only inside the row-encoding hot loop; the
/// schema-default path still goes through the legacy `write_value` so
/// DEFAULT values keep their self-describing tag and remain decodable
/// without consulting a column type.
fn write_value_body(out: &mut Vec<u8>, v: &Value<'_>, ty: DataType) {
    match (v, ty) {
        (Value::SmallInt(n), DataType::SmallInt) => out.extend_from_slice(&n.to_le_bytes()),
        (Value::Int(n), DataType::Int) => out.extend_from_slice(&n.to_le_bytes()),
        // v7.39 (round 640) — xid / xid8 share the BIGINT body.
        (Value::BigInt(n), DataType::BigInt | DataType::Xid | DataType::Xid8) => {
            out.extend_from_slice(&n.to_le_bytes())
        }
        (Value::Xid(x), DataType::Xid) => out.extend_from_slice(&i64::from(*x).to_le_bytes()),
        (Value::Float(x), DataType::Float) => out.extend_from_slice(&x.to_le_bytes()),
        (Value::Real(x), DataType::Real) => out.extend_from_slice(&x.to_le_bytes()),
        (Value::Bool(b), DataType::Bool) => out.push(u8::from(*b)),
        (
            Value::Text(s) | Value::BpChar(s),
            // v7.39 (round 291) — a NAME column holds a Value::Text; the
            // body is byte-identical to TEXT and only the schema's type
            // tag differs.
            DataType::Text | DataType::Varchar(_) | DataType::Char(_) | DataType::Name,
        ) => {
            write_str(out, s);
        }
        (
            Value::Vector(v),
            DataType::Vector {
                encoding: VecEncoding::F32,
                ..
            },
        ) => {
            let dim = u32::try_from(v.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            for x in v.iter() {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        // v6.0.1: SQ8 dense body — [u32 dim][f32 min][f32 max]
        // [u8 * dim]. Self-describes its length so v6 readers
        // walking rows of a v6 catalog stay aligned even if the
        // declared column dim drifts (defensive, not normally
        // possible since CREATE TABLE pins the dim).
        (
            Value::Sq8Vector(q),
            DataType::Vector {
                encoding: VecEncoding::Sq8,
                ..
            },
        ) => {
            let dim = u32::try_from(q.bytes.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            out.extend_from_slice(&q.min.to_le_bytes());
            out.extend_from_slice(&q.max.to_le_bytes());
            out.extend_from_slice(&q.bytes);
        }
        // v6.0.3: halfvec dense body — [u32 dim][u16 LE * dim].
        // The raw u16 bytes already live in `h.bytes` little-
        // endian, so we just splat them.
        (
            Value::HalfVector(h),
            DataType::Vector {
                encoding: VecEncoding::F16,
                ..
            },
        ) => {
            let dim = u32::try_from(h.dim()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            out.extend_from_slice(&h.bytes);
        }
        (
            Value::Numeric {
                scaled,
                scale,
                kind,
            },
            DataType::Numeric { .. },
        ) => {
            // v7.38 (read01, T6.P4) — a form byte prefixes the body (FILE_VERSION
            // 56+): 0 = finite (scaled + scale follow), 1 = NaN, 2 = +Infinity,
            // 3 = -Infinity (specials carry no body). The value's OWN scale is
            // written (not the column's), so an unconstrained NUMERIC keeps each
            // value's scale across persist (a constrained column coerces on
            // insert, so the two already agree there).
            match kind {
                crate::NumericKind::Finite => {
                    // v7.39 (round 271) — scale widened to u16. A scale
                    // that still fits a byte keeps form 0 byte-for-byte,
                    // so nothing already on disk changes shape; only the
                    // scales that could not previously EXIST take the new
                    // form 4. No migration, no version fence.
                    if let Ok(narrow) = u8::try_from(*scale) {
                        out.push(0);
                        out.extend_from_slice(&scaled.to_le_bytes());
                        out.push(narrow);
                    } else {
                        out.push(4);
                        out.extend_from_slice(&scaled.to_le_bytes());
                        out.extend_from_slice(&scale.to_le_bytes());
                    }
                }
                crate::NumericKind::NaN => out.push(1),
                crate::NumericKind::PosInf => out.push(2),
                crate::NumericKind::NegInf => out.push(3),
            }
        }
        (Value::Date(d), DataType::Date) => out.extend_from_slice(&d.to_le_bytes()),
        (Value::Timestamp(t), DataType::Timestamp | DataType::Timestamptz) => {
            out.extend_from_slice(&t.to_le_bytes())
        }
        // v7.37.5 β-P2 — INTERVAL fixed 16-byte body: i64 micros +
        // i32 days + i32 months. Field order mirrors PG's binary
        // format (sqlx-postgres receives the same byte sequence).
        // Stored little-endian per SPG's codec convention; the
        // pgwire layer byte-swaps when emitting / consuming the
        // big-endian wire form.
        (
            Value::Interval {
                months,
                days,
                micros,
            },
            DataType::Interval,
        ) => {
            out.extend_from_slice(&micros.to_le_bytes());
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&months.to_le_bytes());
        }
        // v4.9: JSON stores as length-prefixed text; same shape as
        // Text — the type tag lives in the column schema, not the
        // per-cell body.
        (Value::Json(s), DataType::Json | DataType::Jsonb) => write_str(out, s),
        // v7.10.4: BYTEA shares the [u16 len][bytes] shape with
        // Text but writes raw bytes (no UTF-8 invariant).
        // v7.27 (round-21) — BYTEA takes the escaped length: round-14
        // moved TEXT to the escape codec and missed this arm; the
        // twin fired during mailrs's production migration window.
        (Value::Bytes(b), DataType::Bytes) => write_bytes_escaped(out, b),
        // v7.10.9: TEXT[] dense body — [u16 count][per element:
        // u8 null flag + (when non-null) u16 len + utf-8 bytes].
        (Value::TextArray(items), DataType::TextArray) => {
            let count = u16::try_from(items.len()).expect("TEXT[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(s) => {
                        out.push(0);
                        write_bytes_escaped(out, s.as_bytes());
                    }
                }
            }
        }
        // v7.11.12: INT[] dense body — [u16 count][per element:
        // u8 null + (when non-null) i32 LE].
        (Value::IntArray(items), DataType::IntArray) => {
            let count = u16::try_from(items.len()).expect("INT[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(n) => {
                        out.push(0);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                }
            }
        }
        // v7.11.12: BIGINT[] dense body — [u16 count][per element:
        // u8 null + (when non-null) i64 LE].
        (Value::BigIntArray(items), DataType::BigIntArray) => {
            let count = u16::try_from(items.len()).expect("BIGINT[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(n) => {
                        out.push(0);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                }
            }
        }
        // v7.37.5 β-P4: INTERVAL[] schema-aware dense body —
        // [u16 count][per elem: u8 null + (when non-null) i64 LE
        // micros + i32 LE days + i32 LE months]. Field order
        // mirrors the scalar `Value::Interval` codec arm above
        // so a future binary array BIND path can splat both with
        // the same byte layout.
        (Value::IntervalArray(items), DataType::IntervalArray) => {
            let count = u16::try_from(items.len()).expect("INTERVAL[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(span) => {
                        out.push(0);
                        out.extend_from_slice(&span.micros.to_le_bytes());
                        out.extend_from_slice(&span.days.to_le_bytes());
                        out.extend_from_slice(&span.months.to_le_bytes());
                    }
                }
            }
        }
        // v7.37.5 γ — array-of-scalar dense body. Same envelope
        // as INTERVAL[]: [u16 count][per elem: u8 null + (non-null)
        // scalar body]. Per-element body matches the scalar's LE
        // codec form so a future binary array BIND path can splat.
        (Value::BoolArray(items), DataType::BoolArray) => {
            let count = u16::try_from(items.len()).expect("BOOL[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(b) => {
                        out.push(0);
                        out.push(u8::from(*b));
                    }
                }
            }
        }
        (Value::SmallIntArray(items), DataType::SmallIntArray) => {
            let count = u16::try_from(items.len()).expect("SMALLINT[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(n) => {
                        out.push(0);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                }
            }
        }
        (Value::FloatArray(items), DataType::FloatArray) => {
            let count = u16::try_from(items.len()).expect("FLOAT[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(x) => {
                        out.push(0);
                        out.extend_from_slice(&x.to_le_bytes());
                    }
                }
            }
        }
        (Value::NumericArray(items), DataType::NumericArray) => {
            let count = u16::try_from(items.len()).expect("NUMERIC[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some((scaled, scale)) => {
                        // v7.39 (round 271) — element flag 0 keeps the
                        // one-byte scale; flag 2 carries the u16 one.
                        if let Ok(narrow) = u8::try_from(*scale) {
                            out.push(0);
                            out.extend_from_slice(&scaled.to_le_bytes());
                            out.push(narrow);
                        } else {
                            out.push(2);
                            out.extend_from_slice(&scaled.to_le_bytes());
                            out.extend_from_slice(&scale.to_le_bytes());
                        }
                    }
                }
            }
        }
        (Value::DateArray(items), DataType::DateArray) => {
            let count = u16::try_from(items.len()).expect("DATE[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(d) => {
                        out.push(0);
                        out.extend_from_slice(&d.to_le_bytes());
                    }
                }
            }
        }
        (Value::TimestampArray(items), DataType::TimestampArray)
        | (Value::TimestamptzArray(items), DataType::TimestamptzArray) => {
            let count = u16::try_from(items.len()).expect("TIMESTAMP[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(t) => {
                        out.push(0);
                        out.extend_from_slice(&t.to_le_bytes());
                    }
                }
            }
        }
        (Value::UuidArray(items), DataType::UuidArray) => {
            let count = u16::try_from(items.len()).expect("UUID[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(b) => {
                        out.push(0);
                        out.extend_from_slice(&b[..]);
                    }
                }
            }
        }
        (Value::JsonArray(items), DataType::JsonArray)
        | (Value::JsonbArray(items), DataType::JsonbArray)
        | (Value::VarcharArray(items), DataType::VarcharArray)
        | (Value::CharArray(items), DataType::CharArray) => {
            let count = u16::try_from(items.len()).expect("string-array ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(s) => {
                        out.push(0);
                        write_bytes_escaped(out, s.as_bytes());
                    }
                }
            }
        }
        (Value::BytesArray(items), DataType::BytesArray) => {
            let count = u16::try_from(items.len()).expect("BYTEA[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(b) => {
                        out.push(0);
                        write_bytes_escaped(out, b);
                    }
                }
            }
        }
        // v7.37.5 ε — geometry fixed-width dense bodies. Field
        // order is LE (matches the SPG storage convention) and
        // mirrors PG's binary format for compatibility with a
        // future binary BIND path.
        (Value::Point(p), DataType::Point) => {
            out.extend_from_slice(&p.x.to_le_bytes());
            out.extend_from_slice(&p.y.to_le_bytes());
        }
        (Value::Lseg(p1, p2), DataType::Lseg) => {
            out.extend_from_slice(&p1.x.to_le_bytes());
            out.extend_from_slice(&p1.y.to_le_bytes());
            out.extend_from_slice(&p2.x.to_le_bytes());
            out.extend_from_slice(&p2.y.to_le_bytes());
        }
        (Value::PgBox(ur, ll), DataType::PgBox) => {
            out.extend_from_slice(&ur.x.to_le_bytes());
            out.extend_from_slice(&ur.y.to_le_bytes());
            out.extend_from_slice(&ll.x.to_le_bytes());
            out.extend_from_slice(&ll.y.to_le_bytes());
        }
        (Value::Line { a, b, c }, DataType::Line) => {
            out.extend_from_slice(&a.to_le_bytes());
            out.extend_from_slice(&b.to_le_bytes());
            out.extend_from_slice(&c.to_le_bytes());
        }
        (Value::Circle { center, radius }, DataType::Circle) => {
            out.extend_from_slice(&center.x.to_le_bytes());
            out.extend_from_slice(&center.y.to_le_bytes());
            out.extend_from_slice(&radius.to_le_bytes());
        }
        // v7.37.5 ε — Path: [u8 closed flag][u32 count][Point*n].
        (Value::Path { points, closed }, DataType::Path) => {
            out.push(u8::from(*closed));
            let count = u32::try_from(points.len()).expect("PATH ≤ 4G points");
            out.extend_from_slice(&count.to_le_bytes());
            for p in points {
                out.extend_from_slice(&p.x.to_le_bytes());
                out.extend_from_slice(&p.y.to_le_bytes());
            }
        }
        // v7.37.5 ε — Polygon: [u32 count][Point*n].
        (Value::Polygon(points), DataType::Polygon) => {
            let count = u32::try_from(points.len()).expect("POLYGON ≤ 4G points");
            out.extend_from_slice(&count.to_le_bytes());
            for p in points {
                out.extend_from_slice(&p.x.to_le_bytes());
                out.extend_from_slice(&p.y.to_le_bytes());
            }
        }
        // v7.37.5 ζ-A — network. Body: u8 family + u8 bits + 16 B addr.
        // Cidr shares the Inet body shape (the CIDR invariant is
        // enforced at parse/coerce, not on disk).
        (Value::Inet { family, bits, addr }, DataType::Inet)
        | (Value::Cidr { family, bits, addr }, DataType::Cidr) => {
            out.push(*family);
            out.push(*bits);
            out.extend_from_slice(&addr[..]);
        }
        (Value::Macaddr(m), DataType::Macaddr) => out.extend_from_slice(&m[..]),
        (Value::Macaddr8(m), DataType::Macaddr8) => out.extend_from_slice(&m[..]),
        (Value::PgLsn(l), DataType::PgLsn) => out.extend_from_slice(&l.to_le_bytes()),
        // v7.37.5 ζ-A — BitString shared codec for BIT and BIT VARYING.
        // Body: [u32 LE nbits][ceil(nbits/8) bytes packed BE-in-byte].
        (Value::BitString { nbits, bytes }, DataType::Bit(_))
        | (Value::BitString { nbits, bytes }, DataType::BitVarying(_)) => {
            out.extend_from_slice(&nbits.to_le_bytes());
            out.extend_from_slice(bytes);
        }
        // v7.37.5 ζ-A — XML stored as length-prefixed text (same
        // envelope as Text / Json).
        (Value::Xml(s), DataType::Xml) => write_str(out, s),
        // v7.37.5 ζ-A — `"char"` is a single raw byte.
        (Value::Char1(b), DataType::Char1) => out.push(*b),
        // v7.37.5 ζ-A — MONEY[] dense body. Same shape as
        // BigIntArray (per-elem i64 LE cents).
        (Value::MoneyArray(items), DataType::MoneyArray) => {
            let count = u16::try_from(items.len()).expect("MONEY[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(n) => {
                        out.push(0);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                }
            }
        }
        // v7.37.5 δ — Multirange dense body: [u16 count][per range:
        // u8 flags + (if lower present) bound body + (if upper
        // present) bound body]. Bound bodies recurse via
        // write_value (schema-agnostic — bounds carry their own
        // scalar tags). RangeKind is on the DataType slot, not
        // repeated per range.
        (Value::Multirange { kind: _, ranges }, DataType::Multirange(_)) => {
            let count = u16::try_from(ranges.len()).expect("multirange ≤ 65k ranges");
            out.extend_from_slice(&count.to_le_bytes());
            for r in ranges {
                let mut flags: u8 = 0;
                if r.empty {
                    flags |= 0b0000_0001;
                }
                if r.lower.is_some() {
                    flags |= 0b0000_0010;
                }
                if r.upper.is_some() {
                    flags |= 0b0000_0100;
                }
                if r.lower_inc {
                    flags |= 0b0000_1000;
                }
                if r.upper_inc {
                    flags |= 0b0001_0000;
                }
                out.push(flags);
                if let Some(l) = &r.lower {
                    write_value(out, l);
                }
                if let Some(u) = &r.upper {
                    write_value(out, u);
                }
            }
        }
        // v7.12.0: tsvector dense body — see `value_body_encoded_len`
        // for layout. Lexemes are written in their already-sorted order.
        (Value::TsVector(lexs), DataType::TsVector) => write_tsvector_body(out, lexs),
        // v7.12.0: tsquery dense body — prefix-coded tree.
        (Value::TsQuery(ast), DataType::TsQuery) => write_tsquery_body(out, ast),
        // v7.17.0: UUID dense body — raw 16 bytes (RFC 4122 byte
        // order). No length prefix; the type's fixed width makes
        // the codec stateless.
        (Value::Uuid(b), DataType::Uuid) => out.extend_from_slice(&b[..]),
        // v7.17.0 Phase 3.P0-32: TIME dense body — i64 LE
        // microseconds since 00:00:00.
        (Value::Time(us), DataType::Time) => out.extend_from_slice(&us.to_le_bytes()),
        // v7.17.0 Phase 3.P0-33: YEAR dense body — u16 LE.
        (Value::Year(y), DataType::Year) => out.extend_from_slice(&y.to_le_bytes()),
        // v7.17.0 Phase 3.P0-34: TIMETZ dense body — i64 LE us +
        // i32 LE offset_secs.
        (Value::TimeTz { us, offset_secs }, DataType::TimeTz) => {
            out.extend_from_slice(&us.to_le_bytes());
            out.extend_from_slice(&offset_secs.to_le_bytes());
        }
        // v7.17.0 Phase 3.P0-35: MONEY dense body — i64 LE cents.
        (Value::Money(c), DataType::Money) => out.extend_from_slice(&c.to_le_bytes()),
        // v7.17.0 Phase 3.P0-38: range dense body — see
        // value_body_encoded_len for layout. `kind` is implicit
        // from the column DataType.
        (
            Value::Range {
                lower,
                upper,
                lower_inc,
                upper_inc,
                empty,
                ..
            },
            DataType::Range(_),
        ) => {
            let mut flags: u8 = 0;
            if *empty {
                flags |= 0b0000_0001;
            }
            if lower.is_some() {
                flags |= 0b0000_0010;
            }
            if upper.is_some() {
                flags |= 0b0000_0100;
            }
            if *lower_inc {
                flags |= 0b0000_1000;
            }
            if *upper_inc {
                flags |= 0b0001_0000;
            }
            out.push(flags);
            if let Some(l) = lower {
                write_value(out, l);
            }
            if let Some(u) = upper {
                write_value(out, u);
            }
        }
        // v7.17.0 Phase 3.P0-39: hstore dense body — same shape
        // as write_value_body for hstore (no leading tag — that
        // lives on the data type).
        (Value::Hstore(pairs), DataType::Hstore) => write_hstore_body(out, pairs),
        // v7.17.0 Phase 3.P0-40: 2D array dense body.
        (Value::IntArray2D(rows), DataType::IntArray2D) => write_int_2d_body(out, rows),
        (Value::BigIntArray2D(rows), DataType::BigIntArray2D) => write_bigint_2d_body(out, rows),
        (Value::TextArray2D(rows), DataType::TextArray2D) => write_text_2d_body(out, rows),
        (Value::BoolArray2D(rows), DataType::BoolArray2D) => write_bool_2d_body(out, rows),
        // Type mismatch shouldn't happen — `Table::insert` validates
        // value type against column type before pushing. Treat as a
        // bug, not a runtime error.
        (other, ty) => unreachable!(
            "schema-driven encode received mismatched value/type pair: \
             value tag={:?}, column type={:?}",
            other.data_type(),
            ty
        ),
    }
}

/// v7.17.0 Phase 3.P0-38 — length the schema-agnostic
/// `write_value` would emit for `v`. Used by the range codec to
/// pre-size cells. We mirror the tag-byte + body shape from
/// `write_value` rather than serialising to a temp Vec.
fn write_value_encoded_len(v: &Value<'_>) -> usize {
    match v {
        Value::Null => 1,
        Value::SmallInt(_) => 1 + 2,
        Value::Int(_) | Value::Date(_) => 1 + 4,
        Value::BigInt(_)
        | Value::Float(_)
        | Value::Timestamp(_)
        | Value::Time(_)
        | Value::Money(_) => 1 + 8,
        Value::Bool(_) => 1 + 1,
        Value::Year(_) => 1 + 2,
        Value::Text(s) | Value::Json(s) => 1 + 4 + s.len(),
        Value::Bytes(b) => 1 + 4 + b.len(),
        Value::Numeric { .. } => 1 + 1 + 16 + 1, // tag + form + scaled + scale (T6.P4)
        Value::Uuid(_) => 1 + 16,
        Value::TimeTz { .. } => 1 + 12,
        Value::Hstore(pairs) => {
            let mut n = 1 + 4;
            for (k, v) in pairs {
                n += 4 + k.len() + 1;
                if let Some(val) = v {
                    n += 4 + val.len();
                }
            }
            n
        }
        Value::IntArray2D(rows) => {
            let cols = rows.first().map(|r| r.len()).unwrap_or(0);
            1 + 8 + rows.len() * cols * (1 + 4)
        }
        Value::BigIntArray2D(rows) => {
            let cols = rows.first().map(|r| r.len()).unwrap_or(0);
            1 + 8 + rows.len() * cols * (1 + 8)
        }
        Value::BoolArray2D(rows) => {
            let cols = rows.first().map(|r| r.len()).unwrap_or(0);
            1 + 8 + rows.len() * cols
        }
        Value::TextArray2D(rows) => {
            let cols = rows.first().map(|r| r.len()).unwrap_or(0);
            let mut n = 1 + 8 + rows.len() * cols;
            for row in rows {
                for s in row.iter().flatten() {
                    n += 4 + s.len();
                }
            }
            n
        }
        // Range-of-range and other nested cases — not currently
        // representable but defensively measured via the dense
        // body when the data_type is known.
        other => {
            let ty = other.data_type().unwrap_or(DataType::Int);
            1 + value_body_encoded_len(other, ty)
        }
    }
}

pub(crate) fn write_value(out: &mut Vec<u8>, v: &Value<'_>) {
    match v {
        // v7.38 (read01, T9) — a composite is transient (never persisted); the
        // binary codec encodes it as absent (the text protocol renders it via
        // value_to_text and row_to_json converts it to JSON before storage).
        Value::Composite(_) => out.push(0),
        Value::RegClass(..) | Value::RegProc(..) | Value::Tid(..) | Value::Xid(_)
        | Value::Cid(_) => out.push(0),
        Value::Null => out.push(0),
        Value::SmallInt(n) => {
            out.push(7);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Int(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::BigInt(n) => {
            out.push(2);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float(x) => {
            out.push(3);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Real(x) => {
            out.push(32);
            out.extend_from_slice(&x.to_le_bytes());
        }
        // v4.9: JSON shares the tag-4 (Text) on-disk encoding —
        // schema decides which variant comes back on read. The
        // bodies are byte-identical so collapsing the match keeps
        // clippy::match_same_arms quiet.
        Value::Text(s) | Value::Json(s) | Value::BpChar(s) => {
            out.push(4);
            write_str(out, s);
        }
        Value::Bool(b) => {
            out.push(5);
            out.push(u8::from(*b));
        }
        Value::Vector(v) => {
            out.push(6);
            let dim = u32::try_from(v.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            for x in v.iter() {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        // v6.0.1: new tag 11 for an SQ8 cell carried with its full
        // header. Layout matches the dense row body shape so a
        // round-trip through write_value → read_value bit-equals
        // the original `Value::Sq8Vector`.
        Value::Sq8Vector(q) => {
            out.push(11);
            let dim = u32::try_from(q.bytes.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            out.extend_from_slice(&q.min.to_le_bytes());
            out.extend_from_slice(&q.max.to_le_bytes());
            out.extend_from_slice(&q.bytes);
        }
        // v6.0.3: tag 12 for a HalfVector cell.
        // Layout: `[u32 dim][u16 LE × dim]` — bit-identical to the
        // dense row body so `write_value` / `read_value` bit-equal
        // the original `Value::HalfVector`.
        Value::HalfVector(h) => {
            out.push(12);
            let dim = u32::try_from(h.dim()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            out.extend_from_slice(&h.bytes);
        }
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => {
            out.push(8);
            // v7.38 (read01, T6.P4) — form byte after the tag (FILE_VERSION 56+).
            match kind {
                crate::NumericKind::Finite => {
                    // v7.39 (round 271) — scale widened to u16. A scale
                    // that still fits a byte keeps form 0 byte-for-byte,
                    // so nothing already on disk changes shape; only the
                    // scales that could not previously EXIST take the new
                    // form 4. No migration, no version fence.
                    if let Ok(narrow) = u8::try_from(*scale) {
                        out.push(0);
                        out.extend_from_slice(&scaled.to_le_bytes());
                        out.push(narrow);
                    } else {
                        out.push(4);
                        out.extend_from_slice(&scaled.to_le_bytes());
                        out.extend_from_slice(&scale.to_le_bytes());
                    }
                }
                crate::NumericKind::NaN => out.push(1),
                crate::NumericKind::PosInf => out.push(2),
                crate::NumericKind::NegInf => out.push(3),
            }
        }
        // v7.38 (read01, T3.C3) — arbitrary-precision NUMERIC (FILE_VERSION 57+):
        // tag 33, then [neg u8][scale u8][nlimbs u16 LE][limb u32 LE]…
        Value::NumericBig(b) => {
            let (neg, limbs, scale) = b.parts();
            // v7.39 (round 271) — tag 33 keeps its one-byte scale; tag 34
            // is the same layout with a u16 scale, written only when the
            // value needs it.
            out.push(if scale > 255 { 34 } else { 33 });
            out.push(u8::from(neg));
            if let Ok(narrow) = u8::try_from(scale) {
                out.push(narrow);
            } else {
                out.extend_from_slice(&scale.to_le_bytes());
            }
            out.extend_from_slice(&(limbs.len() as u16).to_le_bytes());
            for &l in limbs {
                out.extend_from_slice(&l.to_le_bytes());
            }
        }
        Value::Date(d) => {
            out.push(9);
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Timestamp(t) => {
            out.push(10);
            out.extend_from_slice(&t.to_le_bytes());
        }
        // v7.37.5 β-P2 — schema-less Interval: tag 30 + the same
        // 16-byte body the schema-aware `write_value_body` emits
        // (i64 micros + i32 days + i32 months, LE). Schema-less
        // tag space is independent of the catalog DataType tag
        // space (which uses 34 for INTERVAL); 30 was the next free
        // schema-less slot.
        Value::Interval {
            months,
            days,
            micros,
        } => {
            out.push(30);
            out.extend_from_slice(&micros.to_le_bytes());
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&months.to_le_bytes());
        }
        // v7.10.4: BYTEA — [u8 tag=13_b][u16 len][bytes]. Tag
        // distinct from Text (4) so the schema-agnostic
        // read_value path can disambiguate. (Tag 11 is taken by
        // the WAL `auto_commit_sql` shape elsewhere, hence 14.)
        Value::Bytes(b) => {
            out.push(14);
            write_bytes_escaped(out, b);
        }
        // v7.10.9: TEXT[] — [u8 tag=15][u16 count][per elem: u8
        // null + (if non-null) u16 len + utf-8 bytes].
        Value::TextArray(items) => {
            out.push(15);
            let count = u16::try_from(items.len()).expect("TEXT[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(s) => {
                        out.push(0);
                        write_bytes_escaped(out, s.as_bytes());
                    }
                }
            }
        }
        // v7.11.12: INT[] — tag 16. [u16 count][per elem: u8 null +
        // (if non-null) i32 LE].
        Value::IntArray(items) => {
            out.push(16);
            let count = u16::try_from(items.len()).expect("INT[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(n) => {
                        out.push(0);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                }
            }
        }
        // v7.11.12: BIGINT[] — tag 17. [u16 count][per elem: u8 null +
        // (if non-null) i64 LE].
        Value::BigIntArray(items) => {
            out.push(17);
            let count = u16::try_from(items.len()).expect("BIGINT[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(n) => {
                        out.push(0);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                }
            }
        }
        // v7.37.5 β-P4: INTERVAL[] schema-less — tag 31. Same
        // body shape as the schema-aware arm. Schema-less tags
        // 28/29 already taken by 2D arrays' read path on the
        // schema-aware DataType side (28/29 are read_value
        // arms below); 31 is the next free schema-less slot.
        Value::IntervalArray(items) => {
            out.push(31);
            let count = u16::try_from(items.len()).expect("INTERVAL[] ≤ 65k elements");
            out.extend_from_slice(&count.to_le_bytes());
            for item in items {
                match item {
                    None => out.push(1),
                    Some(span) => {
                        out.push(0);
                        out.extend_from_slice(&span.micros.to_le_bytes());
                        out.extend_from_slice(&span.days.to_le_bytes());
                        out.extend_from_slice(&span.months.to_le_bytes());
                    }
                }
            }
        }
        // v7.37.5 γ — array-of-scalar family. These reach the
        // schema-less path only through corrupt / synthetic
        // catalogs (Range elements; never through normal column
        // storage which is always schema-aware). Until a use
        // case appears, fence with `unreachable!()` so a future
        // accidental call surfaces loud instead of writing a
        // silently-invalid tag.
        Value::BoolArray(_)
        | Value::SmallIntArray(_)
        | Value::FloatArray(_)
        | Value::NumericArray(_)
        | Value::DateArray(_)
        | Value::TimestampArray(_)
        | Value::TimestamptzArray(_)
        | Value::UuidArray(_)
        | Value::JsonArray(_)
        | Value::JsonbArray(_)
        | Value::BytesArray(_)
        | Value::VarcharArray(_)
        | Value::CharArray(_) => unreachable!(
            "v7.37.5 γ array-of-scalar lacks a schema-less codec tag — \
             use schema-aware write_value_body via the column's DataType"
        ),
        // v7.37.5 δ — Multirange is column-typed only (the RangeKind
        // pin lives on the DataType slot). Schema-less path fences
        // for the same reason as the γ array-of-scalar family.
        Value::Multirange { .. } => unreachable!(
            "v7.37.5 δ Multirange lacks a schema-less codec tag — \
             use schema-aware write_value_body via the column's DataType"
        ),
        // v7.37.5 ε — geometry scalars are column-typed only.
        // Schema-less path fences for the same reason as γ / δ.
        Value::Point(_)
        | Value::Lseg(_, _)
        | Value::Path { .. }
        | Value::PgBox(_, _)
        | Value::Polygon(_)
        | Value::Line { .. }
        | Value::Circle { .. } => unreachable!(
            "v7.37.5 ε geometry scalar lacks a schema-less codec tag — \
             use schema-aware write_value_body via the column's DataType"
        ),
        // v7.37.5 ζ-A — network / bit / xml / "char" / money[]
        // column-typed only.
        Value::Inet { .. }
        | Value::Cidr { .. }
        | Value::Macaddr(_)
        | Value::Macaddr8(_)
        | Value::PgLsn(_)
        | Value::BitString { .. }
        | Value::Xml(_)
        | Value::Char1(_)
        | Value::MoneyArray(_) => unreachable!(
            "v7.37.5 ζ-A network/bit/xml/\"char\"/money[] lack a schema-less codec tag — \
             use schema-aware write_value_body via the column's DataType"
        ),
        // v7.12.0: tsvector — tag 18. Body shape matches
        // `write_tsvector_body`.
        Value::TsVector(lexs) => {
            out.push(18);
            write_tsvector_body(out, lexs);
        }
        // v7.12.0: tsquery — tag 19. Body shape matches
        // `write_tsquery_body`.
        Value::TsQuery(ast) => {
            out.push(19);
            write_tsquery_body(out, ast);
        }
        // v7.17.0: UUID — tag 20. Body = raw 16 bytes (RFC 4122
        // byte order).
        Value::Uuid(b) => {
            out.push(20);
            out.extend_from_slice(&b[..]);
        }
        // v7.17.0 Phase 3.P0-32: TIME — tag 21. Body = i64 LE
        // microseconds since 00:00:00.
        Value::Time(us) => {
            out.push(21);
            out.extend_from_slice(&us.to_le_bytes());
        }
        // v7.17.0 Phase 3.P0-33: YEAR — tag 22. Body = u16 LE.
        Value::Year(y) => {
            out.push(22);
            out.extend_from_slice(&y.to_le_bytes());
        }
        // v7.17.0 Phase 3.P0-34: TIMETZ — tag 23. Body = i64 LE
        // us + i32 LE offset_secs.
        Value::TimeTz { us, offset_secs } => {
            out.push(23);
            out.extend_from_slice(&us.to_le_bytes());
            out.extend_from_slice(&offset_secs.to_le_bytes());
        }
        // v7.17.0 Phase 3.P0-35: MONEY — tag 24. Body = i64 LE cents.
        Value::Money(c) => {
            out.push(24);
            out.extend_from_slice(&c.to_le_bytes());
        }
        // v7.17.0 Phase 3.P0-38: range — tag 25. Body =
        // [u8 RangeKind tag][u8 flags][if lower: write_value(lower)]
        // [if upper: write_value(upper)].
        Value::Range {
            kind,
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
        } => {
            out.push(25);
            out.push(kind.tag());
            let mut flags: u8 = 0;
            if *empty {
                flags |= 0b0000_0001;
            }
            if lower.is_some() {
                flags |= 0b0000_0010;
            }
            if upper.is_some() {
                flags |= 0b0000_0100;
            }
            if *lower_inc {
                flags |= 0b0000_1000;
            }
            if *upper_inc {
                flags |= 0b0001_0000;
            }
            out.push(flags);
            if let Some(l) = lower {
                write_value(out, l);
            }
            if let Some(u) = upper {
                write_value(out, u);
            }
        }
        // v7.17.0 Phase 3.P0-39: hstore — tag 26. Body =
        // [u32 count] then per pair `[u32 klen][k bytes][u8 has_val]
        // [if has_val: u32 vlen][v bytes]`.
        Value::Hstore(pairs) => {
            out.push(26);
            write_hstore_body(out, pairs);
        }
        // v7.17.0 Phase 3.P0-40: 2D arrays — tag 27/28/29.
        Value::IntArray2D(rows) => {
            out.push(27);
            write_int_2d_body(out, rows);
        }
        Value::BigIntArray2D(rows) => {
            out.push(28);
            write_bigint_2d_body(out, rows);
        }
        Value::TextArray2D(rows) => {
            out.push(29);
            write_text_2d_body(out, rows);
        }
        // v7.39 (read01 round 75) — bool[][].
        Value::BoolArray2D(rows) => {
            out.push(67);
            write_bool_2d_body(out, rows);
        }
    }
}

/// v7.39 (read01 round 75) — 2-D BOOL writer; one byte per cell (0 = false,
/// 1 = true, 2 = NULL), after the (rows, cols) header the other 2-D bodies use.
fn write_bool_2d_body(out: &mut Vec<u8>, rows: &[Vec<Option<bool>>]) {
    let nrows = u32::try_from(rows.len()).expect("≤ 4G rows");
    let ncols = u32::try_from(rows.first().map(|r| r.len()).unwrap_or(0)).expect("≤ 4G cols");
    out.extend_from_slice(&nrows.to_le_bytes());
    out.extend_from_slice(&ncols.to_le_bytes());
    for row in rows {
        for cell in row {
            out.push(match cell {
                None => 2,
                Some(false) => 0,
                Some(true) => 1,
            });
        }
    }
}

/// v7.17.0 Phase 3.P0-40 — shared 2D INT writer.
fn write_int_2d_body(out: &mut Vec<u8>, rows: &[Vec<Option<i32>>]) {
    let nrows = u32::try_from(rows.len()).expect("≤ 4G rows");
    let ncols = u32::try_from(rows.first().map(|r| r.len()).unwrap_or(0)).expect("≤ 4G cols");
    out.extend_from_slice(&nrows.to_le_bytes());
    out.extend_from_slice(&ncols.to_le_bytes());
    for row in rows {
        for cell in row {
            match cell {
                None => out.push(1),
                Some(n) => {
                    out.push(0);
                    out.extend_from_slice(&n.to_le_bytes());
                }
            }
        }
    }
}

/// v7.17.0 Phase 3.P0-40 — shared 2D BIGINT writer.
fn write_bigint_2d_body(out: &mut Vec<u8>, rows: &[Vec<Option<i64>>]) {
    let nrows = u32::try_from(rows.len()).expect("≤ 4G rows");
    let ncols = u32::try_from(rows.first().map(|r| r.len()).unwrap_or(0)).expect("≤ 4G cols");
    out.extend_from_slice(&nrows.to_le_bytes());
    out.extend_from_slice(&ncols.to_le_bytes());
    for row in rows {
        for cell in row {
            match cell {
                None => out.push(1),
                Some(n) => {
                    out.push(0);
                    out.extend_from_slice(&n.to_le_bytes());
                }
            }
        }
    }
}

/// v7.17.0 Phase 3.P0-40 — shared 2D TEXT writer. Cells use
/// `[u8 null_flag][if non-null: u32 len][utf-8 bytes]` layout.
fn write_text_2d_body(out: &mut Vec<u8>, rows: &[Vec<Option<String>>]) {
    let nrows = u32::try_from(rows.len()).expect("≤ 4G rows");
    let ncols = u32::try_from(rows.first().map(|r| r.len()).unwrap_or(0)).expect("≤ 4G cols");
    out.extend_from_slice(&nrows.to_le_bytes());
    out.extend_from_slice(&ncols.to_le_bytes());
    for row in rows {
        for cell in row {
            match cell {
                None => out.push(1),
                Some(s) => {
                    out.push(0);
                    let l = u32::try_from(s.len()).expect("≤ 4 GiB cell");
                    out.extend_from_slice(&l.to_le_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
            }
        }
    }
}

/// v7.17.0 Phase 3.P0-39 — shared hstore body writer.
fn write_hstore_body(out: &mut Vec<u8>, pairs: &[(String, Option<String>)]) {
    let count = u32::try_from(pairs.len()).expect("hstore ≤ u32::MAX pairs");
    out.extend_from_slice(&count.to_le_bytes());
    for (k, v) in pairs {
        let klen = u32::try_from(k.len()).expect("hstore key ≤ 4 GiB");
        out.extend_from_slice(&klen.to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        match v {
            None => out.push(0),
            Some(val) => {
                out.push(1);
                let vlen = u32::try_from(val.len()).expect("hstore val ≤ 4 GiB");
                out.extend_from_slice(&vlen.to_le_bytes());
                out.extend_from_slice(val.as_bytes());
            }
        }
    }
}

/// v7.12.0: shared tsvector body writer (used by both dense and
/// schema-agnostic codecs).
fn write_tsvector_body(out: &mut Vec<u8>, lexs: &[TsLexeme]) {
    let count = u16::try_from(lexs.len()).expect("tsvector ≤ 65k lexemes");
    out.extend_from_slice(&count.to_le_bytes());
    for l in lexs {
        // v7.27 — escaped length (codec sweep, round-21).
        write_bytes_escaped(out, l.word.as_bytes());
        let plen = u16::try_from(l.positions.len()).expect("tsvector pos count ≤ 65k");
        out.extend_from_slice(&plen.to_le_bytes());
        for p in &l.positions {
            out.extend_from_slice(&p.to_le_bytes());
        }
        out.push(l.weight);
    }
}

/// v7.12.0: shared tsquery body writer. Prefix-coded tree: each
/// node starts with `[u8 tag]` then a tag-specific payload. Tags:
/// 0=Term, 1=And, 2=Or, 3=Not, 4=Phrase.
fn write_tsquery_body(out: &mut Vec<u8>, ast: &TsQueryAst) {
    match ast {
        TsQueryAst::Term { word, weight_mask } => {
            out.push(0);
            // v7.27 — escaped length (codec sweep, round-21).
            write_bytes_escaped(out, word.as_bytes());
            out.push(*weight_mask);
        }
        TsQueryAst::And(a, b) => {
            out.push(1);
            write_tsquery_body(out, a);
            write_tsquery_body(out, b);
        }
        TsQueryAst::Or(a, b) => {
            out.push(2);
            write_tsquery_body(out, a);
            write_tsquery_body(out, b);
        }
        TsQueryAst::Not(x) => {
            out.push(3);
            write_tsquery_body(out, x);
        }
        TsQueryAst::Phrase {
            left,
            right,
            distance,
        } => {
            out.push(4);
            out.extend_from_slice(&distance.to_le_bytes());
            write_tsquery_body(out, left);
            write_tsquery_body(out, right);
        }
    }
}

/// v7.12.0: byte length that `write_tsquery_body` would emit.
fn tsquery_encoded_len(ast: &TsQueryAst) -> usize {
    match ast {
        TsQueryAst::Term { word, .. } => 1 + 2 + word.len() + 1,
        TsQueryAst::And(a, b) | TsQueryAst::Or(a, b) => {
            1 + tsquery_encoded_len(a) + tsquery_encoded_len(b)
        }
        TsQueryAst::Not(x) => 1 + tsquery_encoded_len(x),
        TsQueryAst::Phrase { left, right, .. } => {
            1 + 2 + tsquery_encoded_len(left) + tsquery_encoded_len(right)
        }
    }
}

pub(crate) fn write_u16(out: &mut Vec<u8>, n: u16) {
    out.extend_from_slice(&n.to_le_bytes());
}
pub(crate) fn write_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}
/// v7.37.15 (Epic W slice 1) — u64 LE write, symmetric to
/// [`Cursor::read_u64`]. Used by the row-redo codec to carry a
/// row's stable [`RowId`](crate::row_header::RowId) and writer
/// version (`xmin`/`xmax`).
pub(crate) fn write_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}
/// v7.23 (mailrs round-14) — sentinel for the escape form of the
/// short-string codec: a u16 length of `0xFFFF` means "the REAL
/// length follows as a u32". Strings of length `>= 0xFFFF` take the
/// escape form (including exactly 65 535, so the sentinel is
/// unambiguous within v46+ payloads); shorter strings keep the
/// 2-byte header — zero overhead for identifiers and typical text.
/// Pre-v46 catalogs (and pre-V3 segments) may legitimately contain
/// a plain length of 0xFFFF, so DECODING is gated on the container
/// version (`Cursor::codec_version`); encoding always emits the v46
/// form because every new container carries the new version mark.
pub(crate) const STR_LEN_ESCAPE: u16 = u16::MAX;

/// v7.27 (round-21) — escaped length for RAW BYTE payloads (BYTEA
/// cells, TEXT[] elements when paired with their own validity
/// rules): same sentinel scheme as [`write_str`], decoding gated on
/// codec_version >= 47.
fn write_bytes_escaped(out: &mut Vec<u8>, b: &[u8]) {
    if b.len() >= STR_LEN_ESCAPE as usize {
        let len = u32::try_from(b.len()).expect("cell fits in u32 (4 GiB cap)");
        write_u16(out, STR_LEN_ESCAPE);
        write_u32(out, len);
    } else {
        write_u16(out, b.len() as u16);
    }
    out.extend_from_slice(b);
}

/// v7.37.6-B — serialize a `TableSchema.partition_role` payload.
/// `None` writes the single tag byte `0`; presence variants
/// (Parent / Range / Default) follow the layout pinned in the
/// `FILE_VERSION 49` docstring. Symmetrical reader:
/// [`read_partition_role`].
pub(crate) fn write_partition_role(out: &mut Vec<u8>, role: Option<&crate::PartitionRole>) {
    use crate::{PartitionKind, PartitionRole};
    match role {
        None => out.push(0),
        Some(PartitionRole::Parent {
            kind,
            key_column_positions,
            index_template_sources,
        }) => {
            out.push(1);
            let kind_tag: u8 = match kind {
                PartitionKind::Range => 0,
                // v7.37.16 (16.1 / 16.2) — new partition strategies.
                PartitionKind::List => 1,
                PartitionKind::Hash => 2,
            };
            out.push(kind_tag);
            write_u16(
                out,
                u16::try_from(key_column_positions.len()).expect("≤ 65k partition key columns"),
            );
            for &pos in key_column_positions {
                write_u16(out, u16::try_from(pos).expect("≤ 65k columns/table"));
            }
            write_u16(
                out,
                u16::try_from(index_template_sources.len())
                    .expect("≤ 65k partition index templates"),
            );
            for src in index_template_sources {
                write_str(out, src.as_str());
            }
        }
        Some(PartitionRole::Range {
            parent_name,
            lower,
            upper,
        }) => {
            out.push(2);
            write_str(out, parent_name.as_str());
            write_partition_bound(out, lower);
            write_partition_bound(out, upper);
        }
        Some(PartitionRole::Default { parent_name }) => {
            out.push(3);
            write_str(out, parent_name.as_str());
        }
        // v7.37.16 (16.1) — LIST child role on disk: tag=4,
        // parent_name + values count + each value via
        // write_partition_bound (reuses BigInt/Int/SmallInt/Date/Text
        // codec added in 16.6).
        Some(PartitionRole::List {
            parent_name,
            values,
        }) => {
            out.push(4);
            write_str(out, parent_name.as_str());
            write_u16(
                out,
                u16::try_from(values.len()).expect("≤ 65k LIST partition values"),
            );
            for v in values {
                write_partition_bound(out, v);
            }
        }
        // v7.37.16 (16.2) — HASH child role on disk: tag=5,
        // parent_name + u32 modulus + u32 remainder.
        Some(PartitionRole::Hash {
            parent_name,
            modulus,
            remainder,
        }) => {
            out.push(5);
            write_str(out, parent_name.as_str());
            out.extend_from_slice(&modulus.to_le_bytes());
            out.extend_from_slice(&remainder.to_le_bytes());
        }
    }
}

fn write_partition_bound(out: &mut Vec<u8>, bound: &crate::PartitionBound) {
    use crate::PartitionBound;
    match bound {
        PartitionBound::MinValue => out.push(0),
        PartitionBound::MaxValue => out.push(1),
        PartitionBound::TimestampTz(micros) => {
            out.push(2);
            out.extend_from_slice(&micros.to_le_bytes());
        }
        // v7.37.16 (16.6) — new type tags for extended PartitionBound
        // variants. Round-trip-stable on disk so a v7.37.16 catalog
        // restored on a later version still decodes correctly.
        PartitionBound::BigInt(n) => {
            out.push(3);
            out.extend_from_slice(&n.to_le_bytes());
        }
        PartitionBound::Int(n) => {
            out.push(4);
            out.extend_from_slice(&n.to_le_bytes());
        }
        PartitionBound::SmallInt(n) => {
            out.push(5);
            out.extend_from_slice(&n.to_le_bytes());
        }
        PartitionBound::Date(days) => {
            out.push(6);
            out.extend_from_slice(&days.to_le_bytes());
        }
        PartitionBound::Text(s) => {
            out.push(7);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
}

pub(crate) fn read_partition_role(
    cur: &mut Cursor<'_>,
) -> Result<Option<crate::PartitionRole>, StorageError> {
    use crate::{PartitionKind, PartitionRole};
    let tag = cur.read_u8()?;
    match tag {
        0 => Ok(None),
        1 => {
            let kind_tag = cur.read_u8()?;
            let kind = match kind_tag {
                0 => PartitionKind::Range,
                // v7.37.16 (16.1 / 16.2) — round-trip the new strategies.
                1 => PartitionKind::List,
                2 => PartitionKind::Hash,
                other => {
                    return Err(StorageError::Corrupt(format!(
                        "partition_role Parent: unknown kind tag {other}"
                    )));
                }
            };
            let key_count = cur.read_u16()? as usize;
            let mut key_column_positions = Vec::with_capacity(key_count);
            for _ in 0..key_count {
                key_column_positions.push(cur.read_u16()? as usize);
            }
            let tmpl_count = cur.read_u16()? as usize;
            let mut index_template_sources = Vec::with_capacity(tmpl_count);
            for _ in 0..tmpl_count {
                index_template_sources.push(cur.read_str()?);
            }
            Ok(Some(PartitionRole::Parent {
                kind,
                key_column_positions,
                index_template_sources,
            }))
        }
        2 => {
            let parent_name = cur.read_str()?;
            let lower = read_partition_bound(cur)?;
            let upper = read_partition_bound(cur)?;
            Ok(Some(PartitionRole::Range {
                parent_name,
                lower,
                upper,
            }))
        }
        3 => {
            let parent_name = cur.read_str()?;
            Ok(Some(PartitionRole::Default { parent_name }))
        }
        // v7.37.16 (16.1) — LIST child role from disk.
        4 => {
            let parent_name = cur.read_str()?;
            let n = cur.read_u16()? as usize;
            let mut values = Vec::with_capacity(n);
            for _ in 0..n {
                values.push(read_partition_bound(cur)?);
            }
            Ok(Some(PartitionRole::List {
                parent_name,
                values,
            }))
        }
        // v7.37.16 (16.2) — HASH child role from disk.
        5 => {
            let parent_name = cur.read_str()?;
            let modulus = cur.read_u32()?;
            let remainder = cur.read_u32()?;
            Ok(Some(PartitionRole::Hash {
                parent_name,
                modulus,
                remainder,
            }))
        }
        other => Err(StorageError::Corrupt(format!(
            "partition_role: unknown role tag {other}"
        ))),
    }
}

fn read_partition_bound(cur: &mut Cursor<'_>) -> Result<crate::PartitionBound, StorageError> {
    use crate::PartitionBound;
    let tag = cur.read_u8()?;
    match tag {
        0 => Ok(PartitionBound::MinValue),
        1 => Ok(PartitionBound::MaxValue),
        2 => Ok(PartitionBound::TimestampTz(cur.read_i64()?)),
        // v7.37.16 (16.6) — extended PartitionBound variants.
        // Tags 3..=7 added 2026-06-30; lower-tier (0..=2) untouched
        // so pre-v7.37.16 catalogs decode unchanged.
        3 => Ok(PartitionBound::BigInt(cur.read_i64()?)),
        4 => Ok(PartitionBound::Int(cur.read_i32()?)),
        5 => Ok(PartitionBound::SmallInt(cur.read_i16()?)),
        6 => Ok(PartitionBound::Date(cur.read_i32()?)),
        7 => {
            let len = cur.read_u32()? as usize;
            let bytes = cur.read_bytes(len)?;
            let s = alloc::string::String::from_utf8(bytes.clone()).map_err(|e| {
                StorageError::Corrupt(format!("partition_bound Text: invalid UTF-8: {e}"))
            })?;
            Ok(PartitionBound::Text(s))
        }
        other => Err(StorageError::Corrupt(format!(
            "partition_bound: unknown tag {other}"
        ))),
    }
}

pub(crate) fn write_str(out: &mut Vec<u8>, s: &str) {
    if s.len() >= STR_LEN_ESCAPE as usize {
        // Real mail bodies / document text routinely exceed 64 KiB
        // (mailrs round-14: the old `fits in u16` expect PANICKED —
        // after the INSERT was acknowledged — at the next snapshot
        // encode).
        let len = u32::try_from(s.len()).expect("text fits in u32 (4 GiB cap)");
        write_u16(out, STR_LEN_ESCAPE);
        write_u32(out, len);
    } else {
        write_u16(out, s.len() as u16);
    }
    out.extend_from_slice(s.as_bytes());
}

/// v7.12.4 — long-string variant: `[u32 LE len][bytes]`. For
/// payloads that can plausibly exceed 64 KiB (notably PL/pgSQL
/// function bodies). Identifiers + short text continue to use
/// the u16 [`write_str`] codec.
pub(crate) fn write_str_long(out: &mut Vec<u8>, s: &str) {
    let len = u32::try_from(s.len()).expect("function body fits in u32");
    write_u32(out, len);
    out.extend_from_slice(s.as_bytes());
}

/// Serialise an [`IndexKey`] using the v9 tagged codec. `read_index_key`
/// is the inverse. v8 catalogs never wrote index keys (`BTree` entries were
/// rebuilt from `Table::rows`), so this codec is v9+ only.
pub(crate) fn write_index_key(out: &mut Vec<u8>, key: &IndexKey) {
    match key {
        IndexKey::Int(n) => {
            out.push(INDEX_KEY_TAG_INT);
            out.extend_from_slice(&n.to_le_bytes());
        }
        IndexKey::Text(s) => {
            out.push(INDEX_KEY_TAG_TEXT);
            write_str(out, s);
        }
        IndexKey::Bool(b) => {
            out.push(INDEX_KEY_TAG_BOOL);
            out.push(u8::from(*b));
        }
        IndexKey::Uuid(b) => {
            out.push(INDEX_KEY_TAG_UUID);
            out.extend_from_slice(&b[..]);
        }
    }
}

pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pub(crate) pos: usize,
    /// v7.23/v7.27 — the container's codec version (catalog
    /// FILE_VERSION, or the segment magic mapped onto it). Gates
    /// length-escape decoding: >= 46 strings escape via
    /// [`STR_LEN_ESCAPE`], >= 47 BYTEA / TEXT[] elements / ts
    /// lexemes escape too. 0 = legacy (plain u16 everywhere —
    /// 0xFFFF is a legitimate length there).
    pub(crate) codec_version: u8,
}

impl<'a> Cursor<'a> {
    pub(crate) const fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            codec_version: 0,
        }
    }

    /// v7.23/v7.27 — builder for version-gated escape decoding.
    pub(crate) const fn with_codec_version(mut self, v: u8) -> Self {
        self.codec_version = v;
        self
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], StorageError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| StorageError::Corrupt(format!("length overflow taking {n} bytes")))?;
        if end > self.buf.len() {
            return Err(StorageError::Corrupt(format!(
                "unexpected EOF at offset {} (wanted {n} more bytes)",
                self.pos
            )));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8, StorageError> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn read_u16(&mut self) -> Result<u16, StorageError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    pub(crate) fn read_u32(&mut self) -> Result<u32, StorageError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    pub(crate) fn read_i32(&mut self) -> Result<i32, StorageError> {
        let s = self.take(4)?;
        Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    /// v7.37.16 (16.6) — i16 LE read for SMALLINT partition bound.
    pub(crate) fn read_i16(&mut self) -> Result<i16, StorageError> {
        let s = self.take(2)?;
        Ok(i16::from_le_bytes([s[0], s[1]]))
    }
    /// v7.37.16 (16.6) — borrowing byte read for TEXT partition bound
    /// (variable-length payload). Returns a `Vec<u8>` rather than a
    /// borrow to avoid lifetime threading into the Result.
    pub(crate) fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>, StorageError> {
        let s = self.take(n)?;
        Ok(s.to_vec())
    }
    /// v6.7.2 — u64 LE read for the per-table `hot_tier_bytes`
    /// catalog appendix.
    pub(crate) fn read_u64(&mut self) -> Result<u64, StorageError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }
    pub(crate) fn read_i64(&mut self) -> Result<i64, StorageError> {
        let s = self.take(8)?;
        let arr: [u8; 8] = s.try_into().expect("checked");
        Ok(i64::from_le_bytes(arr))
    }
    pub(crate) fn read_f64(&mut self) -> Result<f64, StorageError> {
        let s = self.take(8)?;
        let arr: [u8; 8] = s.try_into().expect("checked");
        Ok(f64::from_le_bytes(arr))
    }
    pub(crate) fn read_f32(&mut self) -> Result<f32, StorageError> {
        let s = self.take(4)?;
        Ok(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    /// v7.27 — length field with the >=47 escape (BYTEA cells,
    /// TEXT[] elements, ts lexemes/terms).
    pub(crate) fn read_len_escaped_v47(&mut self) -> Result<usize, StorageError> {
        let short = self.read_u16()?;
        if self.codec_version >= 47 && short == STR_LEN_ESCAPE {
            Ok(self.read_u32()? as usize)
        } else {
            Ok(short as usize)
        }
    }

    /// v7.27 — string whose length uses the >=47 escape (TEXT[]
    /// elements, ts lexemes/terms — payloads that were plain u16
    /// through v46).
    pub(crate) fn read_str_escaped_v47(&mut self) -> Result<String, StorageError> {
        let len = self.read_len_escaped_v47()?;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| StorageError::Corrupt("invalid UTF-8 in cell payload".into()))
    }

    pub(crate) fn read_str(&mut self) -> Result<String, StorageError> {
        let short = self.read_u16()?;
        let len = if self.codec_version >= 46 && short == STR_LEN_ESCAPE {
            // v7.23 escape form — real length follows as u32.
            self.read_u32()? as usize
        } else {
            short as usize
        };
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| StorageError::Corrupt("invalid UTF-8 in identifier or text".into()))
    }

    /// v7.12.4 — long-string variant for payloads written via
    /// [`write_str_long`] (u32-length prefix). Used for PL/pgSQL
    /// function bodies which can plausibly exceed 64 KiB.
    pub(crate) fn read_str_long(&mut self) -> Result<String, StorageError> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| StorageError::Corrupt("invalid UTF-8 in long-string payload".into()))
    }

    /// Parse an [`IndexKey`] emitted by `write_index_key` (v9 tagged
    /// codec). Returns `StorageError::Corrupt` on unknown tag or
    /// truncated payload.
    pub(crate) fn read_index_key(&mut self) -> Result<IndexKey, StorageError> {
        let tag = self.read_u8()?;
        match tag {
            INDEX_KEY_TAG_INT => Ok(IndexKey::Int(self.read_i64()?)),
            INDEX_KEY_TAG_TEXT => Ok(IndexKey::Text(self.read_str()?)),
            INDEX_KEY_TAG_BOOL => Ok(IndexKey::Bool(self.read_u8()? != 0)),
            INDEX_KEY_TAG_UUID => {
                let s = self.take(16)?;
                let mut b = [0u8; 16];
                b.copy_from_slice(s);
                Ok(IndexKey::Uuid(b))
            }
            other => Err(StorageError::Corrupt(format!(
                "unknown index key tag: {other}"
            ))),
        }
    }
    /// Schema-driven dense value decode (`FILE_VERSION` 8). Caller has
    /// already cleared the NULL bit from the row bitmap; we read the
    /// fixed-width body for the given column type. Used inside the row
    /// hot loop; column defaults still go through `read_value` (which
    /// reads its own type tag) so DEFAULT round-trips without a schema.
    pub(crate) fn read_value_body(&mut self, ty: DataType) -> Result<Value<'static>, StorageError> {
        match ty {
            DataType::SmallInt => {
                let s = self.take(2)?;
                Ok(Value::SmallInt(i16::from_le_bytes([s[0], s[1]])))
            }
            DataType::Int => Ok(Value::Int(self.read_i32()?)),
            // v7.39 (round 640) — xid8 rides in a BigInt cell; so does
            // xid, but it reads back as the Value::Xid round 512 already
            // gave the type, so a stored column and a `'5'::xid` literal
            // are the same thing to everything downstream.
            DataType::BigInt | DataType::Xid8 => Ok(Value::BigInt(self.read_i64()?)),
            DataType::Xid => Ok(Value::Xid(self.read_i64()? as u32)),
            DataType::Float => Ok(Value::Float(self.read_f64()?)),
            DataType::Real => Ok(Value::Real(self.read_f32()?)),
            DataType::Bool => Ok(Value::Bool(self.read_u8()? != 0)),
            DataType::Text | DataType::Varchar(_) | DataType::Name => {
                Ok(Value::Text(Cow::Owned(self.read_str()?)))
            }
            // v7.38 (read01, T11) — a CHAR(n) column reads back as bpchar.
            DataType::Char(_) => Ok(Value::BpChar(Cow::Owned(self.read_str()?))),
            DataType::Vector {
                encoding: VecEncoding::F32,
                ..
            } => {
                let dim = self.read_u32()? as usize;
                let mut v = Vec::with_capacity(dim);
                for _ in 0..dim {
                    let bytes: [u8; 4] = self.take(4)?.try_into().expect("checked");
                    v.push(f32::from_le_bytes(bytes));
                }
                Ok(Value::Vector(Cow::Owned(v)))
            }
            DataType::Vector {
                encoding: VecEncoding::Sq8,
                ..
            } => {
                let dim = self.read_u32()? as usize;
                let min = self.read_f32()?;
                let max = self.read_f32()?;
                let bytes = self.take(dim)?.to_vec();
                Ok(Value::Sq8Vector(quantize::Sq8Vector { min, max, bytes }))
            }
            DataType::Vector {
                encoding: VecEncoding::F16,
                ..
            } => {
                let dim = self.read_u32()? as usize;
                let bytes = self.take(dim * 2)?.to_vec();
                Ok(Value::HalfVector(halfvec::HalfVector { bytes }))
            }
            DataType::Numeric { .. } => {
                // v7.38 (read01, T6.P4) — FILE_VERSION 56+ prefixes a form byte;
                // older catalogs stored a bare finite (scaled + scale).
                if self.codec_version >= 56 {
                    match self.read_u8()? {
                        0 => {
                            let arr: [u8; 16] = self.take(16)?.try_into().expect("checked");
                            let scaled = i128::from_le_bytes(arr);
                            let scale = u16::from(self.read_u8()?);
                            Ok(Value::Numeric {
                                scaled,
                                scale,
                                kind: crate::NumericKind::Finite,
                            })
                        }
                        // v7.39 (round 271) — wide-scale finite form.
                        4 => {
                            let arr: [u8; 16] = self.take(16)?.try_into().expect("checked");
                            let scaled = i128::from_le_bytes(arr);
                            let lo = self.read_u8()?;
                            let hi = self.read_u8()?;
                            Ok(Value::Numeric {
                                scaled,
                                scale: u16::from_le_bytes([lo, hi]),
                                kind: crate::NumericKind::Finite,
                            })
                        }
                        1 => Ok(Value::numeric_special(crate::NumericKind::NaN)),
                        2 => Ok(Value::numeric_special(crate::NumericKind::PosInf)),
                        3 => Ok(Value::numeric_special(crate::NumericKind::NegInf)),
                        f => Err(StorageError::Corrupt(alloc::format!(
                            "unknown NUMERIC form byte {f}"
                        ))),
                    }
                } else {
                    let arr: [u8; 16] = self.take(16)?.try_into().expect("checked");
                    let scaled = i128::from_le_bytes(arr);
                    // Pre-form-byte layout; its scale was always a byte.
                    let scale = u16::from(self.read_u8()?);
                    Ok(Value::Numeric {
                        scaled,
                        scale,
                        kind: crate::NumericKind::Finite,
                    })
                }
            }
            DataType::Date => Ok(Value::Date(self.read_i32()?)),
            DataType::Timestamp => Ok(Value::Timestamp(self.read_i64()?)),
            DataType::Timestamptz => Ok(Value::Timestamp(self.read_i64()?)),
            DataType::Jsonb => Ok(Value::Json(Cow::Owned(self.read_str()?))),
            DataType::Interval => {
                // v7.37.5 β-P2 — INTERVAL column read: 16-byte body
                // i64 micros + i32 days + i32 months (PG-byte-equal
                // field order, SPG codec is LE).
                let micros = self.read_i64()?;
                let days = self.read_i32()?;
                let months = self.read_i32()?;
                Ok(Value::Interval {
                    months,
                    days,
                    micros,
                })
            }
            DataType::Json => Ok(Value::Json(Cow::Owned(self.read_str()?))),
            // v7.10.4: BYTEA on-disk is [u16 len][bytes]. Same wire
            // shape as Text, but read as raw Vec<u8>.
            DataType::Bytes => {
                // v7.27 (round-21) — escaped length at >= 47.
                let len = self.read_len_escaped_v47()?;
                let bytes = self.take(len)?.to_vec();
                Ok(Value::Bytes(Cow::Owned(bytes)))
            }
            // v7.10.9: TEXT[] dense body.
            DataType::TextArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<String>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_str_escaped_v47()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "TEXT[] null flag: unknown byte {other}"
                            )));
                        }
                    }
                }
                Ok(Value::TextArray(items))
            }
            // v7.11.12: INT[] dense body.
            DataType::IntArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i32>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_i32()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "INT[] null flag: unknown byte {other}"
                            )));
                        }
                    }
                }
                Ok(Value::IntArray(items))
            }
            // v7.11.12: BIGINT[] dense body.
            DataType::BigIntArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i64>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_i64()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "BIGINT[] null flag: unknown byte {other}"
                            )));
                        }
                    }
                }
                Ok(Value::BigIntArray(items))
            }
            // v7.37.5 β-P4: INTERVAL[] dense body —
            // [u16 count][per elem: u8 null + (non-null) i64 LE
            // micros + i32 LE days + i32 LE months].
            DataType::IntervalArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<IntervalSpan>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => {
                            let micros = self.read_i64()?;
                            let days = self.read_i32()?;
                            let months = self.read_i32()?;
                            items.push(Some(IntervalSpan {
                                months,
                                days,
                                micros,
                            }));
                        }
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "INTERVAL[] null flag: unknown byte {other}"
                            )));
                        }
                    }
                }
                Ok(Value::IntervalArray(items))
            }
            // v7.37.5 γ — array-of-scalar dense bodies. Each
            // reads [u16 count][per elem: u8 null + (non-null)
            // scalar body LE].
            DataType::BoolArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<bool>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_u8()? != 0)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "BOOL[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::BoolArray(items))
            }
            DataType::SmallIntArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i16>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => {
                            let s = self.take(2)?;
                            items.push(Some(i16::from_le_bytes([s[0], s[1]])));
                        }
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "SMALLINT[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::SmallIntArray(items))
            }
            DataType::FloatArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<f64>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_f64()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "FLOAT[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::FloatArray(items))
            }
            DataType::NumericArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<(i128, u16)>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => {
                            let arr: [u8; 16] = self.take(16)?.try_into().expect("checked");
                            let scaled = i128::from_le_bytes(arr);
                            let scale = u16::from(self.read_u8()?);
                            items.push(Some((scaled, scale)));
                        }
                        2 => {
                            let arr: [u8; 16] = self.take(16)?.try_into().expect("checked");
                            let scaled = i128::from_le_bytes(arr);
                            let lo = self.read_u8()?;
                            let hi = self.read_u8()?;
                            items.push(Some((scaled, u16::from_le_bytes([lo, hi]))));
                        }
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "NUMERIC[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::NumericArray(items))
            }
            DataType::DateArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i32>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_i32()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "DATE[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::DateArray(items))
            }
            DataType::TimestampArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i64>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_i64()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "TIMESTAMP[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::TimestampArray(items))
            }
            DataType::TimestamptzArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i64>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_i64()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "TIMESTAMPTZ[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::TimestamptzArray(items))
            }
            DataType::UuidArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<[u8; 16]>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => {
                            let s = self.take(16)?;
                            let mut b = [0u8; 16];
                            b.copy_from_slice(s);
                            items.push(Some(b));
                        }
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "UUID[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::UuidArray(items))
            }
            DataType::JsonArray
            | DataType::JsonbArray
            | DataType::VarcharArray
            | DataType::CharArray => {
                let kind = ty;
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<String>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_str_escaped_v47()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "string-array null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(match kind {
                    DataType::JsonArray => Value::JsonArray(items),
                    DataType::JsonbArray => Value::JsonbArray(items),
                    DataType::VarcharArray => Value::VarcharArray(items),
                    DataType::CharArray => Value::CharArray(items),
                    _ => unreachable!(),
                })
            }
            DataType::BytesArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<Vec<u8>>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => {
                            let len = self.read_len_escaped_v47()?;
                            items.push(Some(self.take(len)?.to_vec()));
                        }
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "BYTEA[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::BytesArray(items))
            }
            // v7.37.5 ε — geometry dense reads. Field order
            // mirrors the schema-aware write arm (LE everywhere).
            DataType::Point => {
                let x = self.read_f64()?;
                let y = self.read_f64()?;
                Ok(Value::Point(Point2D { x, y }))
            }
            DataType::Lseg => {
                let p1x = self.read_f64()?;
                let p1y = self.read_f64()?;
                let p2x = self.read_f64()?;
                let p2y = self.read_f64()?;
                Ok(Value::Lseg(
                    Point2D { x: p1x, y: p1y },
                    Point2D { x: p2x, y: p2y },
                ))
            }
            DataType::PgBox => {
                let urx = self.read_f64()?;
                let ury = self.read_f64()?;
                let llx = self.read_f64()?;
                let lly = self.read_f64()?;
                Ok(Value::PgBox(
                    Point2D { x: urx, y: ury },
                    Point2D { x: llx, y: lly },
                ))
            }
            DataType::Line => {
                let a = self.read_f64()?;
                let b = self.read_f64()?;
                let c = self.read_f64()?;
                Ok(Value::Line { a, b, c })
            }
            DataType::Circle => {
                let cx = self.read_f64()?;
                let cy = self.read_f64()?;
                let radius = self.read_f64()?;
                Ok(Value::Circle {
                    center: Point2D { x: cx, y: cy },
                    radius,
                })
            }
            DataType::Path => {
                let closed = self.read_u8()? != 0;
                let count = self.read_u32()? as usize;
                let mut points = Vec::with_capacity(count);
                for _ in 0..count {
                    let x = self.read_f64()?;
                    let y = self.read_f64()?;
                    points.push(Point2D { x, y });
                }
                Ok(Value::Path { points, closed })
            }
            DataType::Polygon => {
                let count = self.read_u32()? as usize;
                let mut points = Vec::with_capacity(count);
                for _ in 0..count {
                    let x = self.read_f64()?;
                    let y = self.read_f64()?;
                    points.push(Point2D { x, y });
                }
                Ok(Value::Polygon(points))
            }
            // v7.37.5 ζ-A — network / bit / xml / "char" / money[].
            DataType::Inet => {
                let family = self.read_u8()?;
                let bits = self.read_u8()?;
                let mut addr = [0u8; 16];
                addr.copy_from_slice(self.take(16)?);
                Ok(Value::Inet { family, bits, addr })
            }
            DataType::Cidr => {
                let family = self.read_u8()?;
                let bits = self.read_u8()?;
                let mut addr = [0u8; 16];
                addr.copy_from_slice(self.take(16)?);
                Ok(Value::Cidr { family, bits, addr })
            }
            DataType::Macaddr => {
                let mut m = [0u8; 6];
                m.copy_from_slice(self.take(6)?);
                Ok(Value::Macaddr(m))
            }
            DataType::PgLsn => {
                let mut b = [0u8; 8];
                b.copy_from_slice(self.take(8)?);
                Ok(Value::PgLsn(u64::from_le_bytes(b)))
            }
            DataType::Macaddr8 => {
                let mut m = [0u8; 8];
                m.copy_from_slice(self.take(8)?);
                Ok(Value::Macaddr8(m))
            }
            DataType::Bit(_) | DataType::BitVarying(_) => {
                let nbits = self.read_u32()?;
                let nbytes = (nbits as usize).div_ceil(8);
                let bytes = self.take(nbytes)?.to_vec();
                Ok(Value::BitString {
                    nbits,
                    bytes: Cow::Owned(bytes),
                })
            }
            DataType::Xml => Ok(Value::Xml(Cow::Owned(self.read_str()?))),
            DataType::Char1 => Ok(Value::Char1(self.read_u8()?)),
            DataType::MoneyArray => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i64>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_i64()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "MONEY[] null flag: {other}"
                            )));
                        }
                    }
                }
                Ok(Value::MoneyArray(items))
            }
            // v7.37.5 δ — Multirange dense body. Symmetric inverse
            // of the schema-aware write arm: read u16 count, per
            // range read u8 flags and optional bounds via
            // read_value (schema-agnostic).
            DataType::Multirange(kind) => {
                let count = self.read_u16()? as usize;
                let mut ranges: Vec<RangeSpan> = Vec::with_capacity(count);
                for _ in 0..count {
                    let flags = self.read_u8()?;
                    let empty = flags & 0b0000_0001 != 0;
                    let has_lower = flags & 0b0000_0010 != 0;
                    let has_upper = flags & 0b0000_0100 != 0;
                    let lower_inc = flags & 0b0000_1000 != 0;
                    let upper_inc = flags & 0b0001_0000 != 0;
                    let lower = if has_lower {
                        Some(alloc::boxed::Box::new(self.read_value()?))
                    } else {
                        None
                    };
                    let upper = if has_upper {
                        Some(alloc::boxed::Box::new(self.read_value()?))
                    } else {
                        None
                    };
                    ranges.push(RangeSpan {
                        lower,
                        upper,
                        lower_inc,
                        upper_inc,
                        empty,
                    });
                }
                Ok(Value::Multirange { kind, ranges })
            }
            // v7.12.0: tsvector dense body — [u16 lex_count]
            // [per lex: u16 word_len + utf-8 word + u16 pos_count
            // + (u16 LE * pos_count) + u8 weight].
            DataType::TsVector => Ok(Value::TsVector(self.read_tsvector_body()?)),
            DataType::TsQuery => Ok(Value::TsQuery(self.read_tsquery_body()?)),
            // v7.17.0: UUID dense body — raw 16 bytes.
            DataType::Uuid => {
                let s = self.take(16)?;
                let mut b = [0u8; 16];
                b.copy_from_slice(s);
                Ok(Value::Uuid(b))
            }
            // v7.17.0 Phase 3.P0-32: TIME dense body — i64 LE.
            DataType::Time => Ok(Value::Time(self.read_i64()?)),
            // v7.17.0 Phase 3.P0-33: YEAR dense body — u16 LE.
            DataType::Year => Ok(Value::Year(self.read_u16()?)),
            // v7.17.0 Phase 3.P0-34: TIMETZ dense body —
            // i64 LE us + i32 LE offset_secs.
            DataType::TimeTz => {
                let us = self.read_i64()?;
                let offset_secs = self.read_i32()?;
                Ok(Value::TimeTz { us, offset_secs })
            }
            // v7.17.0 Phase 3.P0-35: MONEY dense body — i64 LE cents.
            DataType::Money => Ok(Value::Money(self.read_i64()?)),
            // v7.17.0 Phase 3.P0-39: hstore dense body. Body
            // shape == read_hstore_body.
            DataType::Hstore => Ok(Value::Hstore(self.read_hstore_body()?)),
            // v7.17.0 Phase 3.P0-40: 2D arrays dense body.
            DataType::IntArray2D => Ok(Value::IntArray2D(self.read_int_2d_body()?)),
            DataType::BigIntArray2D => Ok(Value::BigIntArray2D(self.read_bigint_2d_body()?)),
            DataType::TextArray2D => Ok(Value::TextArray2D(self.read_text_2d_body()?)),
            DataType::BoolArray2D => Ok(Value::BoolArray2D(self.read_bool_2d_body()?)),
            // v7.17.0 Phase 3.P0-38: range dense body. Element
            // type is determined by the surrounding RangeKind.
            DataType::Range(kind) => {
                let flags = self.read_u8()?;
                let empty = flags & 0b0000_0001 != 0;
                let has_lower = flags & 0b0000_0010 != 0;
                let has_upper = flags & 0b0000_0100 != 0;
                let lower_inc = flags & 0b0000_1000 != 0;
                let upper_inc = flags & 0b0001_0000 != 0;
                let lower = if has_lower {
                    Some(alloc::boxed::Box::new(self.read_value()?))
                } else {
                    None
                };
                let upper = if has_upper {
                    Some(alloc::boxed::Box::new(self.read_value()?))
                } else {
                    None
                };
                Ok(Value::Range {
                    kind,
                    lower,
                    upper,
                    lower_inc,
                    upper_inc,
                    empty,
                })
            }
        }
    }

    /// v7.17.0 Phase 3.P0-40 — read a 2D INT array body emitted
    /// by `write_int_2d_body`.
    /// v7.39 (read01 round 75) — 2-D BOOL reader; mirrors `write_bool_2d_body`.
    pub(crate) fn read_bool_2d_body(&mut self) -> Result<Vec<Vec<Option<bool>>>, StorageError> {
        let nrows = self.read_u32()? as usize;
        let ncols = self.read_u32()? as usize;
        let mut rows = Vec::with_capacity(nrows);
        for _ in 0..nrows {
            let mut row = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                row.push(match self.read_u8()? {
                    0 => Some(false),
                    1 => Some(true),
                    _ => None,
                });
            }
            rows.push(row);
        }
        Ok(rows)
    }

    pub(crate) fn read_int_2d_body(&mut self) -> Result<Vec<Vec<Option<i32>>>, StorageError> {
        let nrows = self.read_u32()? as usize;
        let ncols = self.read_u32()? as usize;
        let mut rows = Vec::with_capacity(nrows);
        for _ in 0..nrows {
            let mut row = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                let null = self.read_u8()?;
                row.push(if null == 1 {
                    None
                } else {
                    Some(self.read_i32()?)
                });
            }
            rows.push(row);
        }
        Ok(rows)
    }

    /// v7.17.0 Phase 3.P0-40 — read a 2D BIGINT array body.
    pub(crate) fn read_bigint_2d_body(&mut self) -> Result<Vec<Vec<Option<i64>>>, StorageError> {
        let nrows = self.read_u32()? as usize;
        let ncols = self.read_u32()? as usize;
        let mut rows = Vec::with_capacity(nrows);
        for _ in 0..nrows {
            let mut row = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                let null = self.read_u8()?;
                row.push(if null == 1 {
                    None
                } else {
                    Some(self.read_i64()?)
                });
            }
            rows.push(row);
        }
        Ok(rows)
    }

    /// v7.17.0 Phase 3.P0-40 — read a 2D TEXT array body. Each
    /// cell is `[u8 null_flag][if non-null: u32 len + utf-8 bytes]`.
    pub(crate) fn read_text_2d_body(&mut self) -> Result<Vec<Vec<Option<String>>>, StorageError> {
        let nrows = self.read_u32()? as usize;
        let ncols = self.read_u32()? as usize;
        let mut rows = Vec::with_capacity(nrows);
        for _ in 0..nrows {
            let mut row = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                let null = self.read_u8()?;
                if null == 1 {
                    row.push(None);
                } else {
                    let l = self.read_u32()? as usize;
                    let bytes = self.take(l)?.to_vec();
                    let s = String::from_utf8(bytes).map_err(|_| {
                        StorageError::Corrupt("2D TEXT cell is not valid UTF-8".into())
                    })?;
                    row.push(Some(s));
                }
            }
            rows.push(row);
        }
        Ok(rows)
    }

    /// v7.17.0 Phase 3.P0-39 — read a hstore body emitted by
    /// `write_hstore_body`.
    pub(crate) fn read_hstore_body(
        &mut self,
    ) -> Result<Vec<(String, Option<String>)>, StorageError> {
        let count = self.read_u32()? as usize;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let klen = self.read_u32()? as usize;
            let k_bytes = self.take(klen)?.to_vec();
            let k = String::from_utf8(k_bytes)
                .map_err(|_| StorageError::Corrupt("hstore key is not valid UTF-8".into()))?;
            let has_val = self.read_u8()? != 0;
            let v =
                if has_val {
                    let vlen = self.read_u32()? as usize;
                    let v_bytes = self.take(vlen)?.to_vec();
                    Some(String::from_utf8(v_bytes).map_err(|_| {
                        StorageError::Corrupt("hstore value is not valid UTF-8".into())
                    })?)
                } else {
                    None
                };
            out.push((k, v));
        }
        Ok(out)
    }

    /// v7.12.0 — read a tsvector body emitted by `write_tsvector_body`.
    pub(crate) fn read_tsvector_body(&mut self) -> Result<Vec<TsLexeme>, StorageError> {
        let count = self.read_u16()? as usize;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let word = self.read_str_escaped_v47()?;
            let pos_count = self.read_u16()? as usize;
            let mut positions = Vec::with_capacity(pos_count);
            for _ in 0..pos_count {
                positions.push(self.read_u16()?);
            }
            let weight = self.read_u8()?;
            out.push(TsLexeme {
                word,
                positions,
                weight,
            });
        }
        Ok(out)
    }

    /// v7.12.0 — read a tsquery body emitted by `write_tsquery_body`.
    pub(crate) fn read_tsquery_body(&mut self) -> Result<TsQueryAst, StorageError> {
        let tag = self.read_u8()?;
        match tag {
            0 => {
                let word = self.read_str_escaped_v47()?;
                let weight_mask = self.read_u8()?;
                Ok(TsQueryAst::Term { word, weight_mask })
            }
            1 => {
                let a = self.read_tsquery_body()?;
                let b = self.read_tsquery_body()?;
                Ok(TsQueryAst::And(Box::new(a), Box::new(b)))
            }
            2 => {
                let a = self.read_tsquery_body()?;
                let b = self.read_tsquery_body()?;
                Ok(TsQueryAst::Or(Box::new(a), Box::new(b)))
            }
            3 => {
                let x = self.read_tsquery_body()?;
                Ok(TsQueryAst::Not(Box::new(x)))
            }
            4 => {
                let distance = self.read_u16()?;
                let left = self.read_tsquery_body()?;
                let right = self.read_tsquery_body()?;
                Ok(TsQueryAst::Phrase {
                    left: Box::new(left),
                    right: Box::new(right),
                    distance,
                })
            }
            other => Err(StorageError::Corrupt(format!(
                "tsquery: unknown node tag {other}"
            ))),
        }
    }

    pub(crate) fn read_value(&mut self) -> Result<Value<'static>, StorageError> {
        let tag = self.read_u8()?;
        match tag {
            0 => Ok(Value::Null),
            1 => Ok(Value::Int(self.read_i32()?)),
            2 => Ok(Value::BigInt(self.read_i64()?)),
            3 => Ok(Value::Float(self.read_f64()?)),
            32 => Ok(Value::Real(self.read_f32()?)),
            // v7.38 (read01, T3.C3) — arbitrary-precision NUMERIC (form 2).
            33 | 34 => {
                let neg = self.read_u8()? != 0;
                // v7.39 (round 271) — tag 34 is tag 33 with a u16 scale.
                let scale = if tag == 34 {
                    let lo = self.read_u8()?;
                    let hi = self.read_u8()?;
                    u16::from_le_bytes([lo, hi])
                } else {
                    u16::from(self.read_u8()?)
                };
                let nlimbs = self.read_u16()? as usize;
                let mut limbs = alloc::vec::Vec::with_capacity(nlimbs);
                for _ in 0..nlimbs {
                    limbs.push(self.read_u32()?);
                }
                Ok(Value::NumericBig(alloc::boxed::Box::new(
                    crate::bignum::BigNumeric::from_parts(neg, limbs, scale),
                )))
            }
            4 => Ok(Value::Text(Cow::Owned(self.read_str()?))),
            5 => Ok(Value::Bool(self.read_u8()? != 0)),
            6 => {
                let dim = self.read_u32()? as usize;
                let mut v = Vec::with_capacity(dim);
                for _ in 0..dim {
                    let bytes: [u8; 4] = self.take(4)?.try_into().expect("checked");
                    v.push(f32::from_le_bytes(bytes));
                }
                Ok(Value::Vector(Cow::Owned(v)))
            }
            7 => {
                let s = self.take(2)?;
                Ok(Value::SmallInt(i16::from_le_bytes([s[0], s[1]])))
            }
            8 => {
                // v7.38 (read01, T6.P4) — form byte after the tag (FILE_VERSION 56+).
                if self.codec_version >= 56 {
                    match self.read_u8()? {
                        0 => {
                            let arr: [u8; 16] = self.take(16)?.try_into().expect("checked");
                            let scaled = i128::from_le_bytes(arr);
                            let scale = u16::from(self.read_u8()?);
                            Ok(Value::Numeric {
                                scaled,
                                scale,
                                kind: crate::NumericKind::Finite,
                            })
                        }
                        // v7.39 (round 271) — wide-scale finite form.
                        4 => {
                            let arr: [u8; 16] = self.take(16)?.try_into().expect("checked");
                            let scaled = i128::from_le_bytes(arr);
                            let lo = self.read_u8()?;
                            let hi = self.read_u8()?;
                            Ok(Value::Numeric {
                                scaled,
                                scale: u16::from_le_bytes([lo, hi]),
                                kind: crate::NumericKind::Finite,
                            })
                        }
                        1 => Ok(Value::numeric_special(crate::NumericKind::NaN)),
                        2 => Ok(Value::numeric_special(crate::NumericKind::PosInf)),
                        3 => Ok(Value::numeric_special(crate::NumericKind::NegInf)),
                        f => Err(StorageError::Corrupt(alloc::format!(
                            "unknown NUMERIC form byte {f}"
                        ))),
                    }
                } else {
                    let arr: [u8; 16] = self.take(16)?.try_into().expect("checked");
                    let scaled = i128::from_le_bytes(arr);
                    // Pre-form-byte layout; its scale was always a byte.
                    let scale = u16::from(self.read_u8()?);
                    Ok(Value::Numeric {
                        scaled,
                        scale,
                        kind: crate::NumericKind::Finite,
                    })
                }
            }
            9 => Ok(Value::Date(self.read_i32()?)),
            10 => Ok(Value::Timestamp(self.read_i64()?)),
            // v6.0.1: tag 11 — Sq8Vector. Pre-v6 readers fall
            // through to the catch-all and surface
            // `Corrupt("unknown value tag")`, matching the
            // forward-compat fence on the column-type side.
            11 => {
                let dim = self.read_u32()? as usize;
                let min = self.read_f32()?;
                let max = self.read_f32()?;
                let bytes = self.take(dim)?.to_vec();
                Ok(Value::Sq8Vector(quantize::Sq8Vector { min, max, bytes }))
            }
            // v6.0.3: tag 12 — HalfVector. Same forward-compat
            // fence story as tag 11.
            12 => {
                let dim = self.read_u32()? as usize;
                let bytes = self.take(dim * 2)?.to_vec();
                Ok(Value::HalfVector(halfvec::HalfVector { bytes }))
            }
            // v7.10.4: tag 14 — BYTEA. [u16 len][bytes].
            14 => {
                // v7.27 (round-21) — escaped length at >= 47.
                let len = self.read_len_escaped_v47()?;
                let bytes = self.take(len)?.to_vec();
                Ok(Value::Bytes(Cow::Owned(bytes)))
            }
            // v7.10.9: tag 15 — TEXT[]. [u16 count][per elem: u8
            // null + (when non-null) u16 len + utf-8 bytes].
            15 => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<String>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_str_escaped_v47()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "TEXT[] null flag in value tag: unknown byte {other}"
                            )));
                        }
                    }
                }
                Ok(Value::TextArray(items))
            }
            // v7.11.12: tags 16/17 — INT[] / BIGINT[].
            16 => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i32>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_i32()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "INT[] null flag in value tag: unknown byte {other}"
                            )));
                        }
                    }
                }
                Ok(Value::IntArray(items))
            }
            17 => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<i64>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => items.push(Some(self.read_i64()?)),
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "BIGINT[] null flag in value tag: unknown byte {other}"
                            )));
                        }
                    }
                }
                Ok(Value::BigIntArray(items))
            }
            // v7.12.0: tag 18 — tsvector. Body matches the dense
            // form (`read_tsvector_body`).
            18 => Ok(Value::TsVector(self.read_tsvector_body()?)),
            // v7.12.0: tag 19 — tsquery.
            19 => Ok(Value::TsQuery(self.read_tsquery_body()?)),
            // v7.17.0: tag 20 — UUID. Raw 16 bytes.
            20 => {
                let s = self.take(16)?;
                let mut b = [0u8; 16];
                b.copy_from_slice(s);
                Ok(Value::Uuid(b))
            }
            // v7.17.0 Phase 3.P0-32: tag 21 — TIME. i64 LE.
            21 => Ok(Value::Time(self.read_i64()?)),
            // v7.17.0 Phase 3.P0-33: tag 22 — YEAR. u16 LE.
            22 => Ok(Value::Year(self.read_u16()?)),
            // v7.17.0 Phase 3.P0-34: tag 23 — TIMETZ. i64 LE us +
            // i32 LE offset_secs.
            23 => {
                let us = self.read_i64()?;
                let offset_secs = self.read_i32()?;
                Ok(Value::TimeTz { us, offset_secs })
            }
            // v7.17.0 Phase 3.P0-35: tag 24 — MONEY. i64 LE cents.
            24 => Ok(Value::Money(self.read_i64()?)),
            // v7.17.0 Phase 3.P0-39: tag 26 — Hstore. Body shape
            // == read_hstore_body.
            26 => Ok(Value::Hstore(self.read_hstore_body()?)),
            // v7.17.0 Phase 3.P0-40: tag 27/28/29 — 2D arrays.
            27 => Ok(Value::IntArray2D(self.read_int_2d_body()?)),
            67 => Ok(Value::BoolArray2D(self.read_bool_2d_body()?)),
            28 => Ok(Value::BigIntArray2D(self.read_bigint_2d_body()?)),
            29 => Ok(Value::TextArray2D(self.read_text_2d_body()?)),
            // v7.37.5 β-P2: tag 30 — INTERVAL (schema-less). Body
            // mirrors the schema-aware path: i64 LE micros + i32
            // LE days + i32 LE months.
            30 => {
                let micros = self.read_i64()?;
                let days = self.read_i32()?;
                let months = self.read_i32()?;
                Ok(Value::Interval {
                    months,
                    days,
                    micros,
                })
            }
            // v7.37.5 β-P4: tag 31 — INTERVAL[] (schema-less). Body
            // mirrors the schema-aware DataType::IntervalArray read.
            31 => {
                let count = self.read_u16()? as usize;
                let mut items: Vec<Option<IntervalSpan>> = Vec::with_capacity(count);
                for _ in 0..count {
                    match self.read_u8()? {
                        0 => {
                            let micros = self.read_i64()?;
                            let days = self.read_i32()?;
                            let months = self.read_i32()?;
                            items.push(Some(IntervalSpan {
                                months,
                                days,
                                micros,
                            }));
                        }
                        1 => items.push(None),
                        other => {
                            return Err(StorageError::Corrupt(format!(
                                "INTERVAL[] null flag in value tag: unknown byte {other}"
                            )));
                        }
                    }
                }
                Ok(Value::IntervalArray(items))
            }
            // v7.17.0 Phase 3.P0-38: tag 25 — Range.
            // [u8 RangeKind tag][u8 flags][opt lower][opt upper].
            25 => {
                let kt = self.read_u8()?;
                let kind = RangeKind::from_tag(kt)
                    .ok_or_else(|| StorageError::Corrupt(format!("unknown RangeKind tag: {kt}")))?;
                let flags = self.read_u8()?;
                let empty = flags & 0b0000_0001 != 0;
                let has_lower = flags & 0b0000_0010 != 0;
                let has_upper = flags & 0b0000_0100 != 0;
                let lower_inc = flags & 0b0000_1000 != 0;
                let upper_inc = flags & 0b0001_0000 != 0;
                let lower = if has_lower {
                    Some(alloc::boxed::Box::new(self.read_value()?))
                } else {
                    None
                };
                let upper = if has_upper {
                    Some(alloc::boxed::Box::new(self.read_value()?))
                } else {
                    None
                };
                Ok(Value::Range {
                    kind,
                    lower,
                    upper,
                    lower_inc,
                    upper_inc,
                    empty,
                })
            }
            other => Err(StorageError::Corrupt(format!("unknown value tag: {other}"))),
        }
    }

    /// Read an NSW graph that was emitted via `write_nsw_graph`. `m`
    /// is passed in because it was already consumed from the per-
    /// index header. Returns the reconstituted `NswGraph`.
    pub(crate) fn read_nsw_graph(&mut self, m: usize) -> Result<NswGraph, StorageError> {
        let m_max_0 = self.read_u16()? as usize;
        let entry_raw = self.read_u32()?;
        let entry = if entry_raw == u32::MAX {
            None
        } else {
            Some(entry_raw as usize)
        };
        let entry_level = self.read_u8()?;
        let node_count = self.read_u32()? as usize;
        // v5.5.0: levels/per-layer are PV-backed in memory, but the wire
        // format is unchanged — decode element-by-element into a PV via
        // push_mut (transient in-place, no per-element path-copy here since
        // the freshly-built PV is uniquely owned).
        let mut levels: PersistentVec<u8> = PersistentVec::new();
        for _ in 0..node_count {
            levels.push_mut(self.read_u8()?);
        }
        let layer_count = self.read_u8()? as usize;
        let mut layers: Vec<PersistentVec<Vec<u32>>> = Vec::with_capacity(layer_count);
        for _ in 0..layer_count {
            let n = self.read_u32()? as usize;
            let mut per_layer: PersistentVec<Vec<u32>> = PersistentVec::new();
            for _ in 0..n {
                let cnt = self.read_u16()? as usize;
                let mut row: Vec<u32> = Vec::with_capacity(cnt);
                for _ in 0..cnt {
                    row.push(self.read_u32()?);
                }
                per_layer.push_mut(row);
            }
            layers.push(per_layer);
        }
        Ok(NswGraph {
            m,
            m_max_0,
            entry,
            entry_level,
            levels,
            layers,
        })
    }
}
