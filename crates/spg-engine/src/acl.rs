//! v7.39 (read01 round 57) — table privileges: GRANT / REVOKE, `pg_class.relacl`,
//! and enforcement against the session role.
//!
//! Before this, GRANT and REVOKE were swallowed as pg_dump noise, `relacl` was
//! hard-NULL and `has_table_privilege` hard-`true` — all three excused by "SPG
//! is single-user". That excuse died with the RLS epic, which gave sessions a
//! real role (`SET ROLE`) and a real superuser rule.
//!
//! The model, straight from PG:
//!
//! - Every table has an OWNER — whoever ran CREATE TABLE. The owner holds every
//!   privilege implicitly and is the only role that may ALTER / DROP it.
//! - `relacl` is EMPTY until the first GRANT. PG leaves it NULL while only the
//!   owner's implicit privileges apply, then materialises the whole list —
//!   owner's own default entry included — on the first GRANT, and keeps it
//!   thereafter even if every grant is later revoked.
//! - A superuser bypasses every check. SPG's superuser rule is the RLS one:
//!   the default login, and an explicit `SET ROLE admin`, are superuser; any
//!   other `SET ROLE` is privilege-subject. So enforcement only ever bites a
//!   session that has explicitly assumed a non-admin role — byte-identical to a
//!   customer on real PG connected as a superuser.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{
    ColumnName, Expr, GrantObject, GrantStatement, SelectItem, SelectStatement, Statement, TableRef,
};
use spg_storage::{AclItem, TableSchema, priv_bits};

use crate::session::LOGIN_ROLE;
use crate::{Engine, EngineError};

/// The privilege letters PG renders an aclitem with, in its order (`arwdDxtm`).
const PRIV_LETTERS: [(u16, char); 13] = [
    (priv_bits::INSERT, 'a'),
    (priv_bits::SELECT, 'r'),
    (priv_bits::UPDATE, 'w'),
    (priv_bits::DELETE, 'd'),
    (priv_bits::TRUNCATE, 'D'),
    (priv_bits::REFERENCES, 'x'),
    (priv_bits::TRIGGER, 't'),
    (priv_bits::MAINTAIN, 'm'),
    // v7.39 (read01 round 60) — the non-table privileges. PG renders a sequence
    // owner's default as `rwU` and the public schema's as `UC`, which is what
    // this ORDER produces.
    (priv_bits::USAGE, 'U'),
    (priv_bits::CREATE, 'C'),
    (priv_bits::CONNECT, 'c'),
    (priv_bits::TEMPORARY, 'T'),
    (priv_bits::EXECUTE, 'X'),
];

/// The privilege word → bit. `None` for a word PG would reject.
pub(crate) fn priv_from_word(w: &str) -> Option<u16> {
    // A trailing " WITH GRANT OPTION" is legal on every word in the
    // has_table_privilege spelling.
    let bare = w
        .trim()
        .split_once(" WITH ")
        .map_or(w.trim(), |(a, _)| a.trim());
    Some(match bare.to_ascii_uppercase().as_str() {
        "SELECT" => priv_bits::SELECT,
        "INSERT" => priv_bits::INSERT,
        "UPDATE" => priv_bits::UPDATE,
        "DELETE" => priv_bits::DELETE,
        "TRUNCATE" => priv_bits::TRUNCATE,
        "REFERENCES" => priv_bits::REFERENCES,
        "TRIGGER" => priv_bits::TRIGGER,
        "MAINTAIN" => priv_bits::MAINTAIN,
        "USAGE" => priv_bits::USAGE,
        "CREATE" => priv_bits::CREATE,
        "CONNECT" => priv_bits::CONNECT,
        "TEMPORARY" | "TEMP" => priv_bits::TEMPORARY,
        "EXECUTE" => priv_bits::EXECUTE,
        _ => return None,
    })
}

/// The `privilege_type` word for a single bit — `information_schema` spells
/// these out in full.
pub(crate) fn priv_word(bit: u16) -> &'static str {
    match bit {
        priv_bits::SELECT => "SELECT",
        priv_bits::INSERT => "INSERT",
        priv_bits::UPDATE => "UPDATE",
        priv_bits::DELETE => "DELETE",
        priv_bits::TRUNCATE => "TRUNCATE",
        priv_bits::REFERENCES => "REFERENCES",
        priv_bits::TRIGGER => "TRIGGER",
        priv_bits::MAINTAIN => "MAINTAIN",
        priv_bits::USAGE => "USAGE",
        priv_bits::CREATE => "CREATE",
        priv_bits::CONNECT => "CONNECT",
        priv_bits::TEMPORARY => "TEMPORARY",
        priv_bits::EXECUTE => "EXECUTE",
        _ => "",
    }
}

/// Iterate the set bits of a mask in PG's rendering order.
pub(crate) fn priv_iter(mask: u16) -> impl Iterator<Item = u16> {
    PRIV_LETTERS
        .into_iter()
        .filter(move |(bit, _)| mask & *bit != 0)
        .map(|(bit, _)| bit)
}

/// One aclitem's text: `grantee=privs/grantor`, with `*` marking a privilege
/// that carries WITH GRANT OPTION. An empty grantee is PUBLIC (`=r/owner`).
fn render_aclitem(a: &AclItem) -> String {
    let mut s = a.grantee.clone();
    s.push('=');
    for (bit, letter) in PRIV_LETTERS {
        if a.privs & bit != 0 {
            s.push(letter);
            if a.grantable & bit != 0 {
                s.push('*');
            }
        }
    }
    s.push('/');
    s.push_str(&a.grantor);
    s
}

/// An aclitem array as PG prints it, or `None` (SQL NULL) when it is empty —
/// `pg_class.relacl` for a table, `pg_attribute.attacl` for a column.
pub(crate) fn render_acl_list(acl: &[AclItem]) -> Option<String> {
    if acl.is_empty() {
        return None;
    }
    let items: Vec<String> = acl.iter().map(render_aclitem).collect();
    Some(alloc::format!("{{{}}}", items.join(",")))
}

/// `pg_class.relacl` for a table.
pub(crate) fn render_relacl(schema: &TableSchema) -> Option<String> {
    render_acl_list(&schema.acl)
}

/// v7.39 (read01 round 60) — the privileges `roles` hold on the schema
/// (`on_schema`) or the database, straight off the catalog's ACL — with PG's
/// defaults when nothing has been granted yet. Shared by the enforcement gate
/// and the `has_*_privilege` probes, so the two can never drift apart.
pub(crate) fn catalog_object_privs(
    cat: &spg_storage::Catalog,
    on_schema: bool,
    roles: &alloc::collections::BTreeSet<String>,
) -> u16 {
    let acl = if on_schema {
        cat.schema_acl()
    } else {
        cat.database_acl()
    };
    if acl.is_empty() {
        return if on_schema {
            priv_bits::USAGE
        } else {
            priv_bits::CONNECT | priv_bits::TEMPORARY
        };
    }
    let mut held = 0;
    for a in acl {
        if a.grantee.is_empty() || roles.iter().any(|r| a.grantee.eq_ignore_ascii_case(r)) {
            held |= a.privs;
        }
    }
    held
}

/// v7.39 (read01 round 60) — `pg_namespace.nspacl` for `public`. Unlike a
/// table's relacl this is NEVER null: PG ships the schema with PUBLIC holding
/// USAGE, and prints that even before anyone grants anything.
pub(crate) fn render_nspacl(cat: &spg_storage::Catalog) -> String {
    if let Some(rendered) = render_acl_list(cat.schema_acl()) {
        return rendered;
    }
    alloc::format!(
        "{{{o}=UC/{o},=U/{o}}}",
        o = SCHEMA_OWNER_ROLE
    )
}

/// v7.39 (read01 round 59) — the privileges `roles` hold on ONE column: its own
/// ACL, which is a DIFFERENT thing from the table's. PG's rule is that either
/// one suffices, so callers OR the two together.
pub(crate) fn column_privs(
    col: &spg_storage::ColumnSchema,
    roles: &alloc::collections::BTreeSet<String>,
) -> u16 {
    let mut held = 0;
    for a in &col.acl {
        if a.grantee.is_empty() || roles.iter().any(|r| a.grantee.eq_ignore_ascii_case(r)) {
            held |= a.privs;
        }
    }
    held
}

/// The privileges `role` holds on `schema`, ignoring superuser-ness: the
/// owner's implicit ALL, plus any explicit grant to the role or to PUBLIC.
///
/// v7.39 (read01 round 58) — `roles` is the role's EFFECTIVE set: itself plus
/// every role it inherits from (`UserStore::effective_roles`). A grant to a
/// group role reaches its members that way, and a NOINHERIT member's set is
/// just itself, so it does not.
pub(crate) fn privs_of_roles(
    schema: &TableSchema,
    owner: &str,
    roles: &alloc::collections::BTreeSet<String>,
) -> u16 {
    if roles.iter().any(|r| r.eq_ignore_ascii_case(owner)) {
        return priv_bits::ALL;
    }
    let mut held = 0;
    for a in &schema.acl {
        // Empty grantee = PUBLIC: held by every role.
        if a.grantee.is_empty() || roles.iter().any(|r| a.grantee.eq_ignore_ascii_case(r)) {
            held |= a.privs;
        }
    }
    held
}

/// v7.39 (read01 round 59) — what a statement reads from ONE table: either
/// every column (a `SELECT *` reached it) or a specific set. An EMPTY set means
/// the table is read but no column value is — `SELECT count(*)`, which PG allows
/// with nothing but a column privilege somewhere on the table.
#[derive(Default)]
pub(crate) struct ColRead {
    pub all: bool,
    pub cols: alloc::collections::BTreeSet<String>,
}

impl Engine {
    /// The role that owns `schema`. An image written before FILE_VERSION 64
    /// predates roles entirely, so its tables read back as the login role's.
    pub(crate) fn table_owner<'a>(&self, schema: &'a TableSchema) -> &'a str {
        schema.owner.as_deref().unwrap_or(LOGIN_ROLE)
    }

    /// Does the session's effective role hold `wanted` (a bit mask) on `table`?
    /// A superuser always does. An unknown table answers `true` — the caller's
    /// own "relation does not exist" error is the one that should surface.
    pub(crate) fn acl_holds(&self, table: &str, wanted: u16) -> bool {
        if self.is_superuser() {
            return true;
        }
        let Some(t) = self.active_catalog().get(table) else {
            return true;
        };
        let owner = self.table_owner(t.schema()).to_string();
        let roles = self.users.effective_roles(self.current_role());
        privs_of_roles(t.schema(), &owner, &roles) & wanted == wanted
    }

    /// Enforce `wanted` on `table`, PG's message and all.
    pub(crate) fn acl_require(&self, table: &str, wanted: u16) -> Result<(), EngineError> {
        if self.acl_holds(table, wanted) {
            Ok(())
        } else {
            Err(EngineError::Unsupported(alloc::format!(
                "permission denied for table {table}"
            )))
        }
    }

    /// Enforce ownership — what ALTER / DROP / the GRANT itself require.
    pub(crate) fn acl_require_owner(&self, table: &str) -> Result<(), EngineError> {
        if self.is_superuser() {
            return Ok(());
        }
        let Some(t) = self.active_catalog().get(table) else {
            return Ok(());
        };
        if self.table_owner(t.schema()).eq_ignore_ascii_case(self.current_role()) {
            Ok(())
        } else {
            Err(EngineError::Unsupported(alloc::format!(
                "must be owner of table {table}"
            )))
        }
    }

    /// v7.39 (read01 round 61) — `GRANT … ON FUNCTION`, and the
    /// `ON ALL TABLES IN SCHEMA` expansion.
    fn exec_grant_functions_or_all_tables(
        &mut self,
        g: &GrantStatement,
        grant: bool,
    ) -> Result<crate::QueryResult, EngineError> {
        for r in &g.grantees {
            self.acl_check_role_exists(r)?;
        }
        match &g.object {
            GrantObject::Functions(names) => {
                let mut mask = 0u16;
                if g.privileges.is_empty() {
                    mask = priv_bits::ALL_FUNCTION;
                } else {
                    for p in &g.privileges {
                        mask |= if p.word.eq_ignore_ascii_case("ALL") {
                            priv_bits::ALL_FUNCTION
                        } else {
                            priv_from_word(&p.word).ok_or_else(|| {
                                EngineError::Unsupported(alloc::format!(
                                    "unrecognized privilege type \"{}\"",
                                    p.word.to_ascii_lowercase()
                                ))
                            })?
                        };
                    }
                }
                for n in names {
                    if self.active_catalog().functions().get(n).is_none() {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "function {n} does not exist"
                        )));
                    }
                }
                for n in names {
                    self.acl_apply_function(n, mask, &g.grantees, grant, g.grant_option)?;
                }
            }
            _ => {
                // ALL TABLES IN SCHEMA: PG expands it at GRANT time into each
                // table's own relacl. Reporting success and doing nothing (what
                // SPG did) tells a DBA the grant landed when it did not.
                let mut mask = 0u16;
                if g.privileges.is_empty() {
                    mask = priv_bits::ALL;
                } else {
                    for p in &g.privileges {
                        mask |= if p.word.eq_ignore_ascii_case("ALL") {
                            priv_bits::ALL
                        } else {
                            priv_from_word(&p.word).ok_or_else(|| {
                                EngineError::Unsupported(alloc::format!(
                                    "unrecognized privilege type \"{}\"",
                                    p.word.to_ascii_lowercase()
                                ))
                            })?
                        };
                    }
                }
                let tables = self.active_catalog().table_names();
                for t in tables {
                    self.acl_apply(&t, mask, &g.grantees, grant, g.grant_option)?;
                }
            }
        }
        Ok(crate::QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    /// v7.39 (read01 round 60) — `GRANT … ON SEQUENCE / SCHEMA / DATABASE`.
    fn exec_grant_non_table(
        &mut self,
        g: &GrantStatement,
        grant: bool,
    ) -> Result<crate::QueryResult, EngineError> {
        let all_mask = match &g.object {
            GrantObject::Sequences(_) => priv_bits::ALL_SEQUENCE,
            GrantObject::Schemas(_) => priv_bits::ALL_SCHEMA,
            _ => priv_bits::ALL_DATABASE,
        };
        let mut mask = 0u16;
        if g.privileges.is_empty() {
            mask = all_mask;
        } else {
            for p in &g.privileges {
                mask |= if p.word.eq_ignore_ascii_case("ALL") {
                    all_mask
                } else {
                    priv_from_word(&p.word).ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "unrecognized privilege type \"{}\"",
                            p.word.to_ascii_lowercase()
                        ))
                    })?
                };
            }
        }
        for r in &g.grantees {
            self.acl_check_role_exists(r)?;
        }
        match &g.object {
            GrantObject::Sequences(names) => {
                for n in names {
                    if self.active_catalog().sequences().get(n).is_none() {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "relation \"{n}\" does not exist"
                        )));
                    }
                }
                for n in names {
                    self.acl_apply_sequence(n, mask, &g.grantees, grant, g.grant_option)?;
                }
            }
            GrantObject::Schemas(names) => {
                for n in names {
                    if !spg_storage::is_builtin_schema(n) {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "schema \"{n}\" does not exist"
                        )));
                    }
                }
                self.acl_apply_catalog(true, mask, &g.grantees, grant, g.grant_option)?;
            }
            _ => {
                self.acl_apply_catalog(false, mask, &g.grantees, grant, g.grant_option)?;
            }
        }
        Ok(crate::QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    /// v7.39 (read01 round 59) — a column-scoped GRANT / REVOKE. Column grants
    /// live in the COLUMN's own acl (PG `pg_attribute.attacl`) and never touch
    /// the table's `relacl`, which is why a role can hold `SELECT (pub)` while
    /// `has_table_privilege(…, 'SELECT')` stays false.
    pub(crate) fn acl_apply_columns(
        &mut self,
        table: &str,
        mask: u16,
        columns: &[String],
        grantees: &[String],
        grant: bool,
        grant_option: bool,
    ) -> Result<(), EngineError> {
        self.acl_require_owner(table)?;
        let grantor = self.current_role().to_string();
        let cat = self.active_catalog_mut();
        let t = cat.get_mut(table).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("relation \"{table}\" does not exist"))
        })?;
        for cname in columns {
            let Some(col) = t
                .schema_mut()
                .columns
                .iter_mut()
                .find(|sc| sc.name.eq_ignore_ascii_case(cname))
            else {
                continue;
            };
            for g in grantees {
                let at = col.acl.iter().position(|a| a.grantee.eq_ignore_ascii_case(g));
                if grant {
                    match at {
                        Some(i) => {
                            col.acl[i].privs |= mask;
                            if grant_option {
                                col.acl[i].grantable |= mask;
                            }
                        }
                        None => col.acl.push(AclItem {
                            grantee: g.clone(),
                            privs: mask,
                            grantable: if grant_option { mask } else { 0 },
                            grantor: grantor.clone(),
                        }),
                    }
                } else if let Some(i) = at {
                    if grant_option {
                        col.acl[i].grantable &= !mask;
                    } else {
                        col.acl[i].privs &= !mask;
                        col.acl[i].grantable &= !mask;
                    }
                    if col.acl[i].privs == 0 {
                        col.acl.remove(i);
                    }
                }
            }
        }
        Ok(())
    }

    /// v7.39 (read01 round 58) — `GRANT devs TO alice` / `REVOKE devs FROM
    /// alice`. Both the granted roles and the members must exist; PUBLIC cannot
    /// be a member of anything.
    fn exec_role_membership(
        &mut self,
        roles: &[String],
        members: &[String],
        grant: bool,
    ) -> Result<crate::QueryResult, EngineError> {
        for r in roles {
            self.acl_check_role_exists(r)?;
        }
        for m in members {
            self.acl_check_role_exists(m)?;
        }
        for r in roles {
            for m in members {
                if grant {
                    self.users.add_member(r, m);
                } else {
                    self.users.drop_member(r, m);
                }
            }
        }
        Ok(crate::QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    /// A role named in a GRANT must exist. PUBLIC (the empty grantee) always
    /// does; so does the login role, which is not a UserStore entry.
    pub(crate) fn acl_check_role_exists(&self, role: &str) -> Result<(), EngineError> {
        if role.is_empty()
            || role.eq_ignore_ascii_case(LOGIN_ROLE)
            || role.eq_ignore_ascii_case(crate::session::BOOTSTRAP_ROLE)
            || self.users.contains(role)
        {
            return Ok(());
        }
        Err(EngineError::Unsupported(alloc::format!(
            "role \"{role}\" does not exist"
        )))
    }

    /// Apply a GRANT (`grant = true`) or REVOKE to one table's ACL.
    pub(crate) fn acl_apply(
        &mut self,
        table: &str,
        mask: u16,
        grantees: &[String],
        grant: bool,
        grant_option: bool,
    ) -> Result<(), EngineError> {
        self.acl_require_owner(table)?;
        let grantor = self.current_role().to_string();
        let owner = {
            let t = self
                .active_catalog()
                .get(table)
                .ok_or_else(|| EngineError::Unsupported(alloc::format!(
                    "relation \"{table}\" does not exist"
                )))?;
            self.table_owner(t.schema()).to_string()
        };
        let cat = self.active_catalog_mut();
        let t = cat
            .get_mut(table)
            .ok_or_else(|| EngineError::Unsupported(alloc::format!(
                "relation \"{table}\" does not exist"
            )))?;
        let acl = &mut t.schema_mut().acl;
        // PG materialises the whole list — owner's default entry first — the
        // moment the first GRANT lands. A REVOKE against a never-granted table
        // has nothing to take away, so it leaves relacl NULL.
        if acl.is_empty() {
            if !grant {
                return Ok(());
            }
            acl.push(AclItem {
                grantee: owner.clone(),
                privs: priv_bits::ALL,
                grantable: 0,
                grantor: owner.clone(),
            });
        }
        for g in grantees {
            let pos = acl.iter().position(|a| a.grantee.eq_ignore_ascii_case(g));
            if grant {
                match pos {
                    Some(i) => {
                        acl[i].privs |= mask;
                        if grant_option {
                            acl[i].grantable |= mask;
                        }
                    }
                    None => acl.push(AclItem {
                        grantee: g.clone(),
                        privs: mask,
                        grantable: if grant_option { mask } else { 0 },
                        grantor: grantor.clone(),
                    }),
                }
            } else if let Some(i) = pos {
                if grant_option {
                    // `REVOKE GRANT OPTION FOR …` takes away only the right to
                    // re-grant; the privilege itself stays.
                    acl[i].grantable &= !mask;
                } else {
                    acl[i].privs &= !mask;
                    acl[i].grantable &= !mask;
                }
                // An entry with nothing left disappears — except the owner's,
                // which PG keeps for as long as relacl is materialised.
                if acl[i].privs == 0 && !acl[i].grantee.eq_ignore_ascii_case(&owner) {
                    acl.remove(i);
                }
            }
        }
        Ok(())
    }
}

/// Every base table a SELECT READS — the primary FROM, every join, every
/// derived table, every CTE body, and every subquery hiding in an expression.
/// A name that is not a real table (a CTE alias, a meta view) simply answers
/// "privilege held" downstream, so over-collecting is safe and under-collecting
/// is not: a missed subquery would be a way to read a table you were never
/// granted.
pub(crate) fn collect_read_tables(
    stmt: &SelectStatement,
    into: &mut alloc::collections::BTreeSet<String>,
) {
    fn walk_table(t: &TableRef, into: &mut alloc::collections::BTreeSet<String>) {
        if let Some(sub) = &t.lateral_subquery {
            collect_read_tables(sub, into);
            return;
        }
        // The synthetic sources (unnest / generate_series / a table function)
        // are not tables; their argument expressions still are walked.
        if t.unnest_expr.is_some()
            || t.generate_series_args.is_some()
            || t.jsonb_each_text_arg.is_some()
            || t.table_fn_call.is_some()
        {
            return;
        }
        into.insert(t.name.clone());
    }
    fn walk_expr(e: &Expr, into: &mut alloc::collections::BTreeSet<String>) {
        match e {
            Expr::ScalarSubquery(s) => collect_read_tables(s, into),
            Expr::Exists { subquery, .. } => collect_read_tables(subquery, into),
            Expr::InSubquery { expr, subquery, .. } => {
                walk_expr(expr, into);
                collect_read_tables(subquery, into);
            }
            Expr::RowInSubquery { row, subquery, .. }
            | Expr::RowCmpSubquery { row, subquery, .. } => {
                row.iter().for_each(|x| walk_expr(x, into));
                collect_read_tables(subquery, into);
            }
            Expr::Binary { lhs, rhs, .. } => {
                walk_expr(lhs, into);
                walk_expr(rhs, into);
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => walk_expr(expr, into),
            Expr::FunctionCall { args, .. } => args.iter().for_each(|a| walk_expr(a, into)),
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    walk_expr(o, into);
                }
                for (c, v) in branches {
                    walk_expr(c, into);
                    walk_expr(v, into);
                }
                if let Some(x) = else_branch {
                    walk_expr(x, into);
                }
            }
            Expr::InList { expr, list, .. } => {
                walk_expr(expr, into);
                list.iter().for_each(|it| walk_expr(it, into));
            }
            Expr::AnyAll { expr, array, .. } => {
                walk_expr(expr, into);
                walk_expr(array, into);
            }
            Expr::Array(items) => items.iter().for_each(|it| walk_expr(it, into)),
            Expr::ArraySubscript { target, index } => {
                walk_expr(target, into);
                walk_expr(index, into);
            }
            _ => {}
        }
    }
    if let Some(from) = &stmt.from {
        walk_table(&from.primary, into);
        for j in &from.joins {
            walk_table(&j.table, into);
            if let Some(on) = &j.on {
                walk_expr(on, into);
            }
        }
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk_expr(expr, into);
        }
    }
    if let Some(w) = &stmt.where_ {
        walk_expr(w, into);
    }
    if let Some(h) = &stmt.having {
        walk_expr(h, into);
    }
    if let Some(gs) = &stmt.group_by {
        gs.iter().for_each(|g| walk_expr(g, into));
    }
    for o in &stmt.order_by {
        walk_expr(&o.expr, into);
    }
    for (_, peer) in &stmt.unions {
        collect_read_tables(peer, into);
    }
    for cte in &stmt.ctes {
        if let Some(s) = cte.body.as_select() {
            collect_read_tables(s, into);
        }
    }
}

impl Engine {
    /// The privilege gate every statement passes through. A superuser session
    /// skips it entirely, so nothing changes for a customer who never runs
    /// `SET ROLE` — exactly as on PG connected as a superuser.
    pub(crate) fn acl_check_statement(&self, stmt: &Statement) -> Result<(), EngineError> {
        if self.is_superuser() {
            return Ok(());
        }
        let mut reads: alloc::collections::BTreeMap<String, ColRead> =
            alloc::collections::BTreeMap::new();
        match stmt {
            Statement::Select(s) => {
                self.collect_select_reads(s, &mut reads);
            }
            Statement::Insert(i) => {
                // v7.39 (read01 round 59) — an INSERT that names its columns
                // needs INSERT on THOSE columns; one that does not names them
                // all, so only a table-wide grant can carry it.
                let icols: alloc::vec::Vec<String> = i.columns.clone().unwrap_or_default();
                self.acl_require_write_columns(&i.table, &icols, priv_bits::INSERT)?;
                if let Some(src) = &i.select_source {
                    self.collect_select_reads(src, &mut reads);
                }
            }
            Statement::Update(u) => {
                let targets: alloc::vec::Vec<String> =
                    u.assignments.iter().map(|(t, _)| t.clone()).collect();
                self.acl_require_write_columns(&u.table, &targets, priv_bits::UPDATE)?;
                // PG only demands SELECT once the statement READS a value:
                // `UPDATE t SET v='x'` needs UPDATE alone, but any WHERE — or
                // an assignment whose right-hand side reads a column — also
                // needs SELECT, and then only on the columns it reads.
                let mut sub = SelectStatement::default();
                sub.from = Some(spg_sql::ast::FromClause {
                    primary: bare_table_ref(u.table.clone()),
                    joins: alloc::vec::Vec::new(),
                });
                sub.where_ = u.where_.clone();
                for (_, e) in &u.assignments {
                    sub.items.push(SelectItem::Expr {
                        expr: e.clone(),
                        alias: None,
                    });
                }
                if let Some(r) = &u.returning {
                    for item in r {
                        sub.items.push(item.clone());
                    }
                }
                self.collect_select_reads(&sub, &mut reads);
                // The UPDATE target is not "read" merely by being written to.
                if let Some(r) = reads.get(&u.table)
                    && !r.all
                    && r.cols.is_empty()
                {
                    reads.remove(&u.table);
                }
            }
            Statement::Delete(d) => {
                self.acl_require(&d.table, priv_bits::DELETE)?;
                let mut sub = SelectStatement::default();
                sub.from = Some(spg_sql::ast::FromClause {
                    primary: bare_table_ref(d.table.clone()),
                    joins: alloc::vec::Vec::new(),
                });
                sub.where_ = d.where_.clone();
                if let Some(r) = &d.returning {
                    for item in r {
                        sub.items.push(item.clone());
                    }
                }
                self.collect_select_reads(&sub, &mut reads);
                if let Some(r) = reads.get(&d.table)
                    && !r.all
                    && r.cols.is_empty()
                {
                    reads.remove(&d.table);
                }
            }
            Statement::Truncate { tables, .. } => {
                for t in tables {
                    self.acl_require(t, priv_bits::TRUNCATE)?;
                }
            }
            // Only the owner may reshape or destroy a table.
            Statement::DropTable { names, .. } => {
                for n in names {
                    self.acl_require_owner(n)?;
                }
            }
            Statement::AlterTable(a) => self.acl_require_owner(&a.name)?,
            Statement::CreateIndex(c) => self.acl_require_owner(&c.table)?,
            // v7.39 (read01 round 60) — creating an object in the schema needs
            // CREATE on it, which PUBLIC does NOT hold (PG 15 revoked it). So a
            // policy-subject role cannot create a table until it is granted.
            Statement::CreateTable(_)
            | Statement::CreateSequence(_)
            | Statement::CreateView(_)
            | Statement::CreateMaterializedView(_)
            | Statement::CreateType(_) => self.acl_require_schema_create()?,
            _ => {}
        }
        for (t, read) in &reads {
            self.acl_require_read(t, read)?;
        }
        Ok(())
    }

    /// The SELECT-level gate. `acl_check_statement` covers the statement
    /// dispatchers, but the read path has FOUR public entries that hand a
    /// `SelectStatement` straight to the executor (the prepared, arena,
    /// streaming and streaming-prepared fast paths) — and two of those
    /// short-circuit before `exec_select_cancel`. Each is a way into the data,
    /// so each takes the gate: a check that only lives on the "normal" path is
    /// not a check.
    pub(crate) fn acl_check_select(&self, s: &SelectStatement) -> Result<(), EngineError> {
        if self.is_superuser() {
            return Ok(());
        }
        // v7.39 (read01 round 59) — column-aware: a role may hold SELECT on a
        // few columns and nothing table-wide.
        let mut reads: alloc::collections::BTreeMap<String, ColRead> =
            alloc::collections::BTreeMap::new();
        self.collect_select_reads(s, &mut reads);
        for (t, read) in &reads {
            self.acl_require_read(t, read)?;
        }
        Ok(())
    }

    /// `GRANT` / `REVOKE`. Table privileges are real; every other object class
    /// is accepted as a no-op so a pg_dump that grants on schemas, sequences or
    /// functions still restores.
    pub(crate) fn exec_grant(
        &mut self,
        g: &GrantStatement,
        grant: bool,
    ) -> Result<crate::QueryResult, EngineError> {
        if let GrantObject::Roles(roles) = &g.object {
            return self.exec_role_membership(roles, &g.grantees, grant);
        }
        // v7.39 (read01 round 60) — the non-table objects.
        if let GrantObject::Sequences(_) | GrantObject::Schemas(_) | GrantObject::Databases(_) =
            &g.object
        {
            return self.exec_grant_non_table(g, grant);
        }
        // v7.39 (read01 round 61) — functions, and the `ALL TABLES IN SCHEMA`
        // expansion (which used to report success and do nothing).
        if let GrantObject::Functions(_) | GrantObject::AllTablesInSchema = &g.object {
            return self.exec_grant_functions_or_all_tables(g, grant);
        }
        let GrantObject::Tables(tables) = &g.object else {
            return Ok(crate::QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        };
        // A privilege word PG does not know is an error, not a silent skip.
        // v7.39 (read01 round 59) — split the list into the table-wide privileges
        // and the column-scoped ones (`GRANT SELECT (a, b), INSERT (c) ON t`).
        let mut table_mask = 0u16;
        let mut column_masks: alloc::vec::Vec<(u16, &[String])> = alloc::vec::Vec::new();
        if g.privileges.is_empty() {
            table_mask = priv_bits::ALL;
        } else {
            for p in &g.privileges {
                let bit = if p.word.eq_ignore_ascii_case("ALL") {
                    priv_bits::ALL
                } else {
                    priv_from_word(&p.word).ok_or_else(|| {
                        // PG's GRANT wording — no colon, and the unquoted ident
                        // is downcased, exactly as the parser saw it.
                        EngineError::Unsupported(alloc::format!(
                            "unrecognized privilege type \"{}\"",
                            p.word.to_ascii_lowercase()
                        ))
                    })?
                };
                if p.columns.is_empty() {
                    table_mask |= bit;
                } else {
                    column_masks.push((bit, &p.columns));
                }
            }
        }
        for t in tables {
            let Some(tb) = self.active_catalog().get(t) else {
                return Err(EngineError::Unsupported(alloc::format!(
                    "relation \"{t}\" does not exist"
                )));
            };
            // Every named column has to exist, PG's wording and all.
            for (_, cols) in &column_masks {
                for c in *cols {
                    if !tb
                        .schema()
                        .columns
                        .iter()
                        .any(|sc| sc.name.eq_ignore_ascii_case(c))
                    {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "column \"{c}\" of relation \"{t}\" does not exist"
                        )));
                    }
                }
            }
        }
        for r in &g.grantees {
            self.acl_check_role_exists(r)?;
        }
        for t in tables {
            if table_mask != 0 {
                self.acl_apply(t, table_mask, &g.grantees, grant, g.grant_option)?;
            }
            for (bit, cols) in &column_masks {
                self.acl_apply_columns(t, *bit, cols, &g.grantees, grant, g.grant_option)?;
            }
        }
        Ok(crate::QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }
}


impl Engine {
    /// v7.39 (read01 round 59) — the column-aware SELECT gate for ONE table.
    ///
    /// PG's rule, in order: a table-wide SELECT settles it. Otherwise every
    /// column the statement actually READS must carry a column-level SELECT —
    /// and `SELECT *`, which reaches every column, therefore needs every column
    /// granted. A statement that reads NO column value of the table
    /// (`SELECT count(*)`) needs only *some* column privilege on it, which is
    /// exactly `has_any_column_privilege`.
    pub(crate) fn acl_require_read(&self, table: &str, read: &ColRead) -> Result<(), EngineError> {
        if self.acl_holds(table, priv_bits::SELECT) {
            return Ok(());
        }
        let denied = || {
            EngineError::Unsupported(alloc::format!("permission denied for table {table}"))
        };
        let Some(t) = self.active_catalog().get(table) else {
            return Ok(());
        };
        let roles = self.users.effective_roles(self.current_role());
        let cols = &t.schema().columns;
        if read.all {
            // Every column, so every column must be granted.
            return if cols
                .iter()
                .all(|c| column_privs(c, &roles) & priv_bits::SELECT != 0)
                && !cols.is_empty()
            {
                Ok(())
            } else {
                Err(denied())
            };
        }
        if read.cols.is_empty() {
            // count(*) — any column privilege will do.
            return if cols
                .iter()
                .any(|c| column_privs(c, &roles) & priv_bits::SELECT != 0)
            {
                Ok(())
            } else {
                Err(denied())
            };
        }
        for name in &read.cols {
            let Some(c) = cols.iter().find(|c| c.name.eq_ignore_ascii_case(name)) else {
                continue;
            };
            if column_privs(c, &roles) & priv_bits::SELECT == 0 {
                return Err(denied());
            }
        }
        Ok(())
    }

    /// The write-side column gate: `INSERT (id) VALUES …` needs INSERT on `id`,
    /// `UPDATE t SET v = …` needs UPDATE on `v`. A table-wide grant settles it.
    pub(crate) fn acl_require_write_columns(
        &self,
        table: &str,
        columns: &[String],
        wanted: u16,
    ) -> Result<(), EngineError> {
        if self.acl_holds(table, wanted) {
            return Ok(());
        }
        let Some(t) = self.active_catalog().get(table) else {
            return Ok(());
        };
        let roles = self.users.effective_roles(self.current_role());
        // No column list = every column, so the table-wide privilege is the
        // only thing that can carry it — and we already know it is absent.
        if columns.is_empty() {
            return Err(EngineError::Unsupported(alloc::format!(
                "permission denied for table {table}"
            )));
        }
        for name in columns {
            let Some(c) = t
                .schema()
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
            else {
                continue;
            };
            if column_privs(c, &roles) & wanted == 0 {
                return Err(EngineError::Unsupported(alloc::format!(
                    "permission denied for table {table}"
                )));
            }
        }
        Ok(())
    }

    /// v7.39 (read01 round 59) — which columns of which base tables a SELECT
    /// reads. Qualified references resolve through the FROM's alias map;
    /// unqualified ones go to every base table AT THAT LEVEL that has such a
    /// column (in PG an unqualified name that two of them share is an error, so
    /// when the query is legal this is exact). Subqueries recurse with their
    /// own level.
    pub(crate) fn collect_select_reads(
        &self,
        stmt: &SelectStatement,
        into: &mut alloc::collections::BTreeMap<String, ColRead>,
    ) {
        let cat = self.active_catalog();
        // This level's base tables, and the aliases that name them.
        let mut bases: alloc::vec::Vec<(String, Option<String>)> = alloc::vec::Vec::new();
        let mut note = |t: &TableRef, into: &mut alloc::collections::BTreeMap<String, ColRead>, this: &Self| {
            if let Some(sub) = &t.lateral_subquery {
                this.collect_select_reads(sub, into);
                return None;
            }
            if t.unnest_expr.is_some()
                || t.generate_series_args.is_some()
                || t.jsonb_each_text_arg.is_some()
                || t.table_fn_call.is_some()
            {
                return None;
            }
            into.entry(t.name.clone()).or_default();
            Some((t.name.clone(), t.alias.clone()))
        };
        if let Some(from) = &stmt.from {
            if let Some(b) = note(&from.primary, into, self) {
                bases.push(b);
            }
            for j in &from.joins {
                if let Some(b) = note(&j.table, into, self) {
                    bases.push(b);
                }
            }
        }
        let owns = |table: &str, col: &str| -> bool {
            cat.get(table).is_some_and(|t| {
                t.schema()
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(col))
            })
        };
        let mut add_col = |c: &ColumnName, into: &mut alloc::collections::BTreeMap<String, ColRead>| {
            match &c.qualifier {
                Some(q) => {
                    // The qualifier is a table name or an alias of one.
                    let target = bases.iter().find(|(t, a)| {
                        a.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q))
                            || t.eq_ignore_ascii_case(q)
                    });
                    if let Some((t, _)) = target {
                        into.entry(t.clone()).or_default().cols.insert(c.name.clone());
                    }
                }
                None => {
                    for (t, _) in &bases {
                        if owns(t, &c.name) {
                            into.entry(t.clone()).or_default().cols.insert(c.name.clone());
                        }
                    }
                }
            }
        };
        // `SELECT *` reaches every column of every base table at this level.
        if stmt.items.iter().any(|i| matches!(i, SelectItem::Wildcard)) {
            for (t, _) in &bases {
                into.entry(t.clone()).or_default().all = true;
            }
        }
        let mut walk = |e: &Expr, into: &mut alloc::collections::BTreeMap<String, ColRead>| {
            self.walk_expr_reads(e, &mut add_col, into);
        };
        for item in &stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                walk(expr, into);
            }
        }
        if let Some(w) = &stmt.where_ {
            walk(w, into);
        }
        if let Some(h) = &stmt.having {
            walk(h, into);
        }
        if let Some(gs) = &stmt.group_by {
            for g in gs {
                walk(g, into);
            }
        }
        for o in &stmt.order_by {
            walk(&o.expr, into);
        }
        if let Some(from) = &stmt.from {
            for j in &from.joins {
                if let Some(on) = &j.on {
                    walk(on, into);
                }
            }
        }
        for (_, peer) in &stmt.unions {
            self.collect_select_reads(peer, into);
        }
        for cte in &stmt.ctes {
            if let Some(s) = cte.body.as_select() {
                self.collect_select_reads(s, into);
            }
        }
    }

    /// The expression half of `collect_select_reads`: hand every column
    /// reference to `add_col`, and recurse into subqueries with their own level.
    fn walk_expr_reads(
        &self,
        e: &Expr,
        add_col: &mut impl FnMut(&ColumnName, &mut alloc::collections::BTreeMap<String, ColRead>),
        into: &mut alloc::collections::BTreeMap<String, ColRead>,
    ) {
        match e {
            Expr::Column(c) => add_col(c, into),
            Expr::ScalarSubquery(s) => self.collect_select_reads(s, into),
            Expr::Exists { subquery, .. } => self.collect_select_reads(subquery, into),
            Expr::InSubquery { expr, subquery, .. } => {
                self.walk_expr_reads(expr, add_col, into);
                self.collect_select_reads(subquery, into);
            }
            Expr::RowInSubquery { row, subquery, .. }
            | Expr::RowCmpSubquery { row, subquery, .. } => {
                for x in row {
                    self.walk_expr_reads(x, add_col, into);
                }
                self.collect_select_reads(subquery, into);
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.walk_expr_reads(lhs, add_col, into);
                self.walk_expr_reads(rhs, add_col, into);
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => {
                self.walk_expr_reads(expr, add_col, into);
            }
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    self.walk_expr_reads(a, add_col, into);
                }
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.walk_expr_reads(o, add_col, into);
                }
                for (c, v) in branches {
                    self.walk_expr_reads(c, add_col, into);
                    self.walk_expr_reads(v, add_col, into);
                }
                if let Some(x) = else_branch {
                    self.walk_expr_reads(x, add_col, into);
                }
            }
            Expr::InList { expr, list, .. } => {
                self.walk_expr_reads(expr, add_col, into);
                for it in list {
                    self.walk_expr_reads(it, add_col, into);
                }
            }
            Expr::AnyAll { expr, array, .. } => {
                self.walk_expr_reads(expr, add_col, into);
                self.walk_expr_reads(array, add_col, into);
            }
            Expr::Array(items) => {
                for it in items {
                    self.walk_expr_reads(it, add_col, into);
                }
            }
            Expr::ArraySubscript { target, index } => {
                self.walk_expr_reads(target, add_col, into);
                self.walk_expr_reads(index, add_col, into);
            }
            _ => {}
        }
    }
}

/// A minimal `TableRef` naming a base table — the synthetic FROM the UPDATE /
/// DELETE gates build so their WHERE and RETURNING go through the same
/// column-read walker a SELECT does.
fn bare_table_ref(name: String) -> TableRef {
    TableRef {
        name,
        alias: None,
        as_of_segment: None,
        unnest_expr: None,
        unnest_column_aliases: Vec::new(),
        with_ordinality: false,
        generate_series_args: None,
        lateral_subquery: None,
        jsonb_each_text_arg: None,
        table_fn_call: None,
    }
}

// ===========================================================================
// v7.39 (read01 round 60) — the NON-TABLE objects: sequences, the schema, the
// database.
//
// The trap here is that PG's default for these is NOT "nobody holds anything".
// PUBLIC holds USAGE on the `public` schema, and CONNECT + TEMPORARY on the
// database, out of the box — but NOT CREATE on either (PG 15 revoked the
// schema one). A model that started them empty would deny every role's first
// SELECT, and a model that started them full would let any role create tables.
// ===========================================================================

/// PG's owner of the `public` schema — a bootstrap role, not a user.
pub(crate) const SCHEMA_OWNER_ROLE: &str = "pg_database_owner";
/// What PUBLIC holds on the `public` schema before anyone grants anything.
const DEFAULT_SCHEMA_PUBLIC: u16 = priv_bits::USAGE;
/// …and on the database.
const DEFAULT_DATABASE_PUBLIC: u16 = priv_bits::CONNECT | priv_bits::TEMPORARY;

impl Engine {
    /// The privileges `roles` hold on the `public` schema. An owner (the login
    /// role) holds USAGE + CREATE; everyone else gets PG's PUBLIC default until
    /// a GRANT says otherwise.
    pub(crate) fn schema_privs(&self, roles: &alloc::collections::BTreeSet<String>) -> u16 {
        catalog_object_privs(self.active_catalog(), true, roles)
    }

    /// The privileges `roles` hold on the database.
    #[allow(dead_code)]
    pub(crate) fn database_privs(&self, roles: &alloc::collections::BTreeSet<String>) -> u16 {
        catalog_object_privs(self.active_catalog(), false, roles)
    }

    /// The privileges `roles` hold on one sequence: the owner's implicit
    /// `rwU`, plus any explicit grant to them or to PUBLIC.
    pub(crate) fn sequence_privs(
        &self,
        seq: &str,
        roles: &alloc::collections::BTreeSet<String>,
    ) -> u16 {
        let Some(s) = self.active_catalog().sequences().get(seq) else {
            return 0;
        };
        let owner = s.owner.as_deref().unwrap_or(crate::session::LOGIN_ROLE);
        if roles.iter().any(|r| r.eq_ignore_ascii_case(owner)) {
            return priv_bits::ALL_SEQUENCE;
        }
        let mut held = 0;
        for a in &s.acl {
            if a.grantee.is_empty() || roles.iter().any(|r| a.grantee.eq_ignore_ascii_case(r)) {
                held |= a.privs;
            }
        }
        held
    }

    /// Enforce a sequence privilege. PG's message names the SEQUENCE, not the
    /// table: `permission denied for sequence sq`.
    pub(crate) fn acl_require_sequence(&self, seq: &str, wanted: u16) -> Result<(), EngineError> {
        if self.is_superuser() {
            return Ok(());
        }
        if self.active_catalog().sequences().get(seq).is_none() {
            return Ok(());
        }
        let roles = self.users.effective_roles(self.current_role());
        if self.sequence_privs(seq, &roles) & wanted != 0 {
            Ok(())
        } else {
            Err(EngineError::Unsupported(alloc::format!(
                "permission denied for sequence {seq}"
            )))
        }
    }

    /// Enforce CREATE on the schema — what CREATE TABLE / CREATE SEQUENCE / …
    /// need. PUBLIC does NOT hold it by default (PG 15 revoked it), so a
    /// policy-subject role has to be granted it.
    pub(crate) fn acl_require_schema_create(&self) -> Result<(), EngineError> {
        if self.is_superuser() {
            return Ok(());
        }
        let roles = self.users.effective_roles(self.current_role());
        if self.schema_privs(&roles) & priv_bits::CREATE != 0 {
            Ok(())
        } else {
            Err(EngineError::Unsupported(
                "permission denied for schema public".into(),
            ))
        }
    }

    /// GRANT / REVOKE on the schema or the database. Both materialise the whole
    /// list on the first grant, PG's default entries included — the same shape
    /// a table's relacl has.
    pub(crate) fn acl_apply_catalog(
        &mut self,
        on_schema: bool,
        mask: u16,
        grantees: &[String],
        grant: bool,
        grant_option: bool,
    ) -> Result<(), EngineError> {
        // PG's `public` schema is owned by the bootstrap role `pg_database_owner`,
        // not by a user — that is the name its nspacl carries, so SPG uses it too.
        let owner = if on_schema {
            alloc::string::String::from(SCHEMA_OWNER_ROLE)
        } else {
            alloc::string::String::from(self.current_role())
        };
        let (default_public, owner_all) = if on_schema {
            (DEFAULT_SCHEMA_PUBLIC, priv_bits::ALL_SCHEMA)
        } else {
            (DEFAULT_DATABASE_PUBLIC, priv_bits::ALL_DATABASE)
        };
        let cat = self.active_catalog_mut();
        let acl = if on_schema {
            cat.schema_acl_mut()
        } else {
            cat.database_acl_mut()
        };
        if acl.is_empty() {
            // Materialise PG's defaults before layering the grant on top —
            // otherwise the first `GRANT CREATE` would silently REVOKE PUBLIC's
            // implicit USAGE.
            acl.push(AclItem {
                grantee: owner.clone(),
                privs: owner_all,
                grantable: 0,
                grantor: owner.clone(),
            });
            acl.push(AclItem {
                grantee: String::new(),
                privs: default_public,
                grantable: 0,
                grantor: owner.clone(),
            });
        }
        for g in grantees {
            let at = acl.iter().position(|a| a.grantee.eq_ignore_ascii_case(g));
            if grant {
                match at {
                    Some(i) => {
                        acl[i].privs |= mask;
                        if grant_option {
                            acl[i].grantable |= mask;
                        }
                    }
                    None => acl.push(AclItem {
                        grantee: g.clone(),
                        privs: mask,
                        grantable: if grant_option { mask } else { 0 },
                        grantor: owner.clone(),
                    }),
                }
            } else if let Some(i) = at {
                if grant_option {
                    acl[i].grantable &= !mask;
                } else {
                    acl[i].privs &= !mask;
                    acl[i].grantable &= !mask;
                }
                if acl[i].privs == 0 && !acl[i].grantee.eq_ignore_ascii_case(&owner) {
                    acl.remove(i);
                }
            }
        }
        Ok(())
    }

    /// GRANT / REVOKE on a sequence.
    pub(crate) fn acl_apply_sequence(
        &mut self,
        seq: &str,
        mask: u16,
        grantees: &[String],
        grant: bool,
        grant_option: bool,
    ) -> Result<(), EngineError> {
        let grantor = alloc::string::String::from(self.current_role());
        let owner = self
            .active_catalog()
            .sequences()
            .get(seq)
            .and_then(|s| s.owner.clone())
            .unwrap_or_else(|| alloc::string::String::from(crate::session::LOGIN_ROLE));
        let cat = self.active_catalog_mut();
        let s = cat.sequence_mut(seq).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("relation \"{seq}\" does not exist"))
        })?;
        if s.acl.is_empty() {
            if !grant {
                return Ok(());
            }
            s.acl.push(AclItem {
                grantee: owner.clone(),
                privs: priv_bits::ALL_SEQUENCE,
                grantable: 0,
                grantor: owner,
            });
        }
        for g in grantees {
            let at = s.acl.iter().position(|a| a.grantee.eq_ignore_ascii_case(g));
            if grant {
                match at {
                    Some(i) => {
                        s.acl[i].privs |= mask;
                        if grant_option {
                            s.acl[i].grantable |= mask;
                        }
                    }
                    None => s.acl.push(AclItem {
                        grantee: g.clone(),
                        privs: mask,
                        grantable: if grant_option { mask } else { 0 },
                        grantor: grantor.clone(),
                    }),
                }
            } else if let Some(i) = at
                && !s.acl[i].grantee.is_empty()
            {
                s.acl[i].privs &= !mask;
                s.acl[i].grantable &= !mask;
                if s.acl[i].privs == 0 {
                    s.acl.remove(i);
                }
            }
        }
        Ok(())
    }
}

// ===========================================================================
// v7.39 (read01 round 61) — FUNCTION privileges. Round 60's lesson again, in a
// third shape: PG's default is not "empty". EXECUTE is granted to PUBLIC out of
// the box, and `proacl` stays NULL to say so — so a function is callable by
// everyone until someone REVOKEs it FROM PUBLIC.
// ===========================================================================

/// v7.39 (read01 round 61) — how many arguments a stored function declares,
/// out of its `args_repr` (`"(x INT, y TEXT)"`).
pub(crate) fn function_arg_count(args_repr: &str) -> usize {
    let inner = args_repr.trim().trim_start_matches('(').trim_end_matches(')');
    if inner.trim().is_empty() {
        0
    } else {
        inner.split(',').count()
    }
}

/// The privileges `roles` hold on a function: the owner's implicit EXECUTE,
/// plus PG's default grant to PUBLIC when nothing has been granted yet.
pub(crate) fn function_privs(
    def: &spg_storage::FunctionDef,
    roles: &alloc::collections::BTreeSet<String>,
) -> u16 {
    if def.acl.is_empty() {
        // The default: PUBLIC may EXECUTE.
        return priv_bits::EXECUTE;
    }
    let owner = def.owner.as_deref().unwrap_or(crate::session::LOGIN_ROLE);
    if roles.iter().any(|r| r.eq_ignore_ascii_case(owner)) {
        return priv_bits::ALL_FUNCTION;
    }
    let mut held = 0;
    for a in &def.acl {
        if a.grantee.is_empty() || roles.iter().any(|r| a.grantee.eq_ignore_ascii_case(r)) {
            held |= a.privs;
        }
    }
    held
}

impl Engine {
    /// GRANT / REVOKE on a function.
    pub(crate) fn acl_apply_function(
        &mut self,
        name: &str,
        mask: u16,
        grantees: &[String],
        grant: bool,
        grant_option: bool,
    ) -> Result<(), EngineError> {
        let grantor = alloc::string::String::from(self.current_role());
        let owner = self
            .active_catalog()
            .functions()
            .get(name)
            .and_then(|f| f.owner.clone())
            .unwrap_or_else(|| alloc::string::String::from(crate::session::LOGIN_ROLE));
        let cat = self.active_catalog_mut();
        let f = cat.function_mut(name).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("function {name} does not exist"))
        })?;
        if f.acl.is_empty() {
            // Materialise PG's defaults — the owner's EXECUTE and PUBLIC's —
            // before layering the change on top. A `REVOKE … FROM PUBLIC` that
            // started from an empty list would otherwise look like a no-op.
            f.acl.push(AclItem {
                grantee: owner.clone(),
                privs: priv_bits::EXECUTE,
                grantable: 0,
                grantor: owner.clone(),
            });
            f.acl.push(AclItem {
                grantee: String::new(),
                privs: priv_bits::EXECUTE,
                grantable: 0,
                grantor: owner.clone(),
            });
        }
        for g in grantees {
            let at = f.acl.iter().position(|a| a.grantee.eq_ignore_ascii_case(g));
            if grant {
                match at {
                    Some(i) => {
                        f.acl[i].privs |= mask;
                        if grant_option {
                            f.acl[i].grantable |= mask;
                        }
                    }
                    None => f.acl.push(AclItem {
                        grantee: g.clone(),
                        privs: mask,
                        grantable: if grant_option { mask } else { 0 },
                        grantor: grantor.clone(),
                    }),
                }
            } else if let Some(i) = at {
                f.acl[i].privs &= !mask;
                f.acl[i].grantable &= !mask;
                if f.acl[i].privs == 0 && !f.acl[i].grantee.eq_ignore_ascii_case(&owner) {
                    f.acl.remove(i);
                }
            }
        }
        Ok(())
    }
}
