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

use spg_sql::ast::{Expr, GrantObject, GrantStatement, SelectItem, SelectStatement, Statement, TableRef};
use spg_storage::{AclItem, TableSchema, priv_bits};

use crate::session::LOGIN_ROLE;
use crate::{Engine, EngineError};

/// The privilege letters PG renders an aclitem with, in its order (`arwdDxtm`).
const PRIV_LETTERS: [(u16, char); 8] = [
    (priv_bits::INSERT, 'a'),
    (priv_bits::SELECT, 'r'),
    (priv_bits::UPDATE, 'w'),
    (priv_bits::DELETE, 'd'),
    (priv_bits::TRUNCATE, 'D'),
    (priv_bits::REFERENCES, 'x'),
    (priv_bits::TRIGGER, 't'),
    (priv_bits::MAINTAIN, 'm'),
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

/// `pg_class.relacl` for a table: the whole aclitem array as PG prints it, or
/// `None` (SQL NULL) while no GRANT has ever run.
pub(crate) fn render_relacl(schema: &TableSchema) -> Option<String> {
    if schema.acl.is_empty() {
        return None;
    }
    let items: Vec<String> = schema.acl.iter().map(render_aclitem).collect();
    Some(alloc::format!("{{{}}}", items.join(",")))
}

/// The privileges `role` holds on `schema`, ignoring superuser-ness: the
/// owner's implicit ALL, plus any explicit grant to the role or to PUBLIC.
pub(crate) fn privs_of(schema: &TableSchema, owner: &str, role: &str) -> u16 {
    if role.eq_ignore_ascii_case(owner) {
        return priv_bits::ALL;
    }
    let mut held = 0;
    for a in &schema.acl {
        // Empty grantee = PUBLIC: held by every role.
        if a.grantee.is_empty() || a.grantee.eq_ignore_ascii_case(role) {
            held |= a.privs;
        }
    }
    held
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
        privs_of(t.schema(), &owner, self.current_role()) & wanted == wanted
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

    /// A role named in a GRANT must exist. PUBLIC (the empty grantee) always
    /// does; so does the login role, which is not a UserStore entry.
    pub(crate) fn acl_check_role_exists(&self, role: &str) -> Result<(), EngineError> {
        if role.is_empty()
            || role.eq_ignore_ascii_case(LOGIN_ROLE)
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
        let mut reads: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
        match stmt {
            Statement::Select(s) => {
                collect_read_tables(s, &mut reads);
            }
            Statement::Insert(i) => {
                self.acl_require(&i.table, priv_bits::INSERT)?;
                if let Some(src) = &i.select_source {
                    collect_read_tables(src, &mut reads);
                }
            }
            Statement::Update(u) => {
                self.acl_require(&u.table, priv_bits::UPDATE)?;
                // PG only demands SELECT once the statement READS a value:
                // `UPDATE t SET v='x'` needs UPDATE alone, but any WHERE — or
                // an assignment whose right-hand side reads a column — also
                // needs SELECT on the table.
                let reads_values = u.where_.is_some()
                    || u.assignments.iter().any(|(_, e)| expr_reads_column(e))
                    || u.returning.is_some();
                if reads_values {
                    self.acl_require(&u.table, priv_bits::SELECT)?;
                }
                if let Some(w) = &u.where_ {
                    let mut sub = SelectStatement::default();
                    sub.where_ = Some(w.clone());
                    collect_read_tables(&sub, &mut reads);
                }
            }
            Statement::Delete(d) => {
                self.acl_require(&d.table, priv_bits::DELETE)?;
                if d.where_.is_some() || d.returning.is_some() {
                    self.acl_require(&d.table, priv_bits::SELECT)?;
                }
                if let Some(w) = &d.where_ {
                    let mut sub = SelectStatement::default();
                    sub.where_ = Some(w.clone());
                    collect_read_tables(&sub, &mut reads);
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
            _ => {}
        }
        for t in &reads {
            self.acl_require(t, priv_bits::SELECT)?;
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
        let mut reads: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
        collect_read_tables(s, &mut reads);
        for t in &reads {
            self.acl_require(t, priv_bits::SELECT)?;
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
        let GrantObject::Tables(tables) = &g.object else {
            return Ok(crate::QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        };
        // A privilege word PG does not know is an error, not a silent skip.
        let mut mask = 0u16;
        if g.privileges.is_empty() {
            mask = priv_bits::ALL;
        } else {
            for w in &g.privileges {
                let bit = priv_from_word(w).ok_or_else(|| {
                    EngineError::Unsupported(alloc::format!("unrecognized privilege type: \"{w}\""))
                })?;
                mask |= bit;
            }
        }
        for t in tables {
            if self.active_catalog().get(t).is_none() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "relation \"{t}\" does not exist"
                )));
            }
        }
        for r in &g.grantees {
            self.acl_check_role_exists(r)?;
        }
        for t in tables {
            self.acl_apply(t, mask, &g.grantees, grant, g.grant_option)?;
        }
        Ok(crate::QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }
}

/// Does this expression read a column value? Drives PG's rule that an UPDATE
/// which only writes constants needs UPDATE alone, while one that reads needs
/// SELECT too.
fn expr_reads_column(e: &Expr) -> bool {
    let mut found = false;
    fn walk(e: &Expr, found: &mut bool) {
        match e {
            Expr::Column(_) => *found = true,
            Expr::Binary { lhs, rhs, .. } => {
                walk(lhs, found);
                walk(rhs, found);
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => walk(expr, found),
            Expr::FunctionCall { args, .. } => args.iter().for_each(|a| walk(a, found)),
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    walk(o, found);
                }
                for (c, v) in branches {
                    walk(c, found);
                    walk(v, found);
                }
                if let Some(x) = else_branch {
                    walk(x, found);
                }
            }
            _ => {}
        }
    }
    walk(e, &mut found);
    found
}
