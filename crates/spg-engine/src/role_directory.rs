//! 8.0.3 — the one numbering of roles that every catalog reads.
//!
//! `pg_roles` numbered roles one way (`postgres` at 10, the declared users
//! from 11, the session's own identity after them), `pg_get_userbyid` a
//! second way (10 is `postgres`, 11 + n is the n-th user), and every
//! owner column a third: the constant 10. So a table created by `u`
//! reported owner `postgres`, `pg_tables` said `admin`, and `pg_dump`
//! wrote `ALTER TABLE … OWNER TO postgres` — which a PostgreSQL started
//! the way the official image starts it has no role for. sentori's §4.4:
//! a dump of SPG did not restore into PostgreSQL.
//!
//! PostgreSQL gives oid 10 to its bootstrap superuser: the role initdb
//! created, which the official image names after `POSTGRES_USER`. SPG
//! records no creation order, so the bootstrap role here is the first
//! superuser that can log in — on a server started from that image's
//! environment, the one role it created. With none, `postgres` stands in,
//! as it always has.

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use crate::Engine;

/// The bootstrap superuser's oid, in PostgreSQL and here.
pub(crate) const BOOTSTRAP_OID: i64 = 10;

/// PostgreSQL 18.6's predefined roles and their oids, measured with
/// `SELECT oid, rolname FROM pg_roles WHERE oid < 16384`. None can log in
/// or is a superuser. `pg_database_owner` owns `public`.
const PREDEFINED: &[(i64, &str)] = &[
    (3373, "pg_monitor"),
    (3374, "pg_read_all_settings"),
    (3375, "pg_read_all_stats"),
    (3377, "pg_stat_scan_tables"),
    (4200, "pg_signal_backend"),
    (4544, "pg_checkpoint"),
    (4550, "pg_use_reserved_connections"),
    (4569, "pg_read_server_files"),
    (4570, "pg_write_server_files"),
    (4571, "pg_execute_server_program"),
    (6171, "pg_database_owner"),
    (6181, "pg_read_all_data"),
    (6182, "pg_write_all_data"),
    (6304, "pg_create_subscription"),
    (6337, "pg_maintain"),
    (6392, "pg_signal_autovacuum_worker"),
];

/// The oid of `pg_database_owner`, the owner of `public`.
pub(crate) const DATABASE_OWNER_OID: i64 = 6171;

pub(crate) struct RoleEntry {
    pub oid: i64,
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub can_login: bool,
}

pub(crate) struct RoleDirectory {
    /// Ordered by oid.
    entries: Vec<RoleEntry>,
}

impl RoleDirectory {
    /// Every role the engine can name: the declared ones, the predefined
    /// ones, the session's own identity, and any role recorded as an
    /// object's owner — a trust-mode login creates objects under a name
    /// no `CREATE ROLE` declared, and the owner column still has to point
    /// at a row.
    pub(crate) fn of(engine: &Engine) -> Self {
        let users = engine.effective_users();
        let bootstrap = users
            .iter()
            .find(|(_, rec)| rec.superuser && rec.can_login)
            .map(|(name, _)| name);
        let mut entries: Vec<RoleEntry> = Vec::new();
        entries.push(RoleEntry {
            oid: BOOTSTRAP_OID,
            name: String::from(bootstrap.unwrap_or("postgres")),
            superuser: true,
            inherit: true,
            can_login: true,
        });
        let mut next = BOOTSTRAP_OID + 1;
        for (name, rec) in users.iter() {
            if Some(name) != bootstrap {
                entries.push(RoleEntry {
                    oid: next,
                    name: String::from(name),
                    superuser: rec.superuser,
                    inherit: rec.inherit,
                    can_login: rec.can_login,
                });
            }
            next += 1;
        }
        for (oid, name) in PREDEFINED {
            if !users.contains(name) {
                entries.push(RoleEntry {
                    oid: *oid,
                    name: String::from(*name),
                    superuser: false,
                    inherit: true,
                    can_login: false,
                });
            }
        }
        let mut undeclared: BTreeSet<String> = recorded_owners(engine);
        undeclared.insert(String::from(engine.session_user()));
        for name in undeclared {
            if entries.iter().any(|e| e.name == name) {
                continue;
            }
            // Not declared, so nothing restricts it: SPG admits a login it
            // has no record of only when it asks for no password.
            entries.push(RoleEntry {
                oid: next,
                name,
                superuser: true,
                inherit: true,
                can_login: true,
            });
            next += 1;
        }
        entries.sort_by_key(|e| e.oid);
        Self { entries }
    }

    /// A directory for a caller that reads a catalog's COLUMNS and discards
    /// its rows: only the bootstrap role, so no row names anyone else.
    pub(crate) fn for_shape_only() -> Self {
        Self {
            entries: alloc::vec![RoleEntry {
                oid: BOOTSTRAP_OID,
                name: String::from("postgres"),
                superuser: true,
                inherit: true,
                can_login: true,
            }],
        }
    }

    pub(crate) fn entries(&self) -> &[RoleEntry] {
        &self.entries
    }

    pub(crate) fn oid_of(&self, name: &str) -> Option<i64> {
        self.entries.iter().find(|e| e.name == name).map(|e| e.oid)
    }

    pub(crate) fn name_of(&self, oid: i64) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.oid == oid)
            .map(|e| e.name.as_str())
    }

    /// The oid behind an object's recorded owner. An object created before
    /// its owner was recorded belongs to the bootstrap superuser, which is
    /// what PostgreSQL's own catalog objects report.
    pub(crate) fn owner_oid(&self, owner: Option<&str>) -> i64 {
        owner
            .and_then(|o| self.entries.iter().find(|e| e.name == o))
            .map_or(BOOTSTRAP_OID, |e| e.oid)
    }

    /// The name [`Self::owner_oid`] resolves to.
    pub(crate) fn owner_name(&self, owner: Option<&str>) -> &str {
        let oid = self.owner_oid(owner);
        self.name_of(oid).unwrap_or("postgres")
    }
}

/// Every role name the catalog records as an owner.
fn recorded_owners(engine: &Engine) -> BTreeSet<String> {
    let cat = engine.active_catalog();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for name in cat.table_names() {
        if let Some(owner) = cat.get(&name).and_then(|t| t.schema().owner.clone()) {
            out.insert(owner);
        }
    }
    for (_, seq) in cat.sequences_all() {
        if let Some(owner) = &seq.owner {
            out.insert(owner.clone());
        }
    }
    for f in cat.functions().values() {
        if let Some(owner) = &f.owner {
            out.insert(owner.clone());
        }
    }
    out.extend(cat.object_owner_names().map(String::from));
    out
}

impl Engine {
    /// 8.0.3 — record the running role as the owner of a view or a type
    /// just created. `CREATE OR REPLACE` of an existing one keeps its
    /// owner, as in PG.
    pub(crate) fn record_creator(&mut self, kind: spg_storage::NonTableKind, name: &str) {
        if self.active_catalog().object_owner(kind, name).is_some() {
            return;
        }
        let role = String::from(self.current_role());
        self.active_catalog_mut()
            .set_object_owner(kind, name, &role);
    }
}
