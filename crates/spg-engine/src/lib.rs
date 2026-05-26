//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

pub mod aggregate;
pub mod eval;
pub mod users;

pub use crate::users::{Role, UserError, UserStore};

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{
    BinOp, ColumnDef, ColumnName, ColumnTypeName, CreateIndexStatement, CreateTableStatement,
    CreateUserStatement, Expr, FromClause, IndexMethod, InsertStatement, JoinKind, Literal,
    SelectItem, SelectStatement, Statement, UnOp, UnionKind,
};
use spg_sql::parser::{self, ParseError};
use spg_storage::{
    Catalog, ColumnSchema, DataType, IndexKey, Row, StorageError, Table, TableSchema, Value,
};

use crate::eval::{EvalContext, EvalError};

/// Result of executing one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// DDL or DML succeeded.
    ///
    /// `affected` is the row count for `INSERT` and 0 elsewhere.
    /// `modified_catalog` tells the server whether this statement
    /// caused the *committed* catalog to change — it's the signal to
    /// snapshot/audit. False for `BEGIN`/`ROLLBACK`, false for writeful
    /// statements executed inside a transaction (those only touch the
    /// shadow), and true for `COMMIT` and for writes outside a TX.
    CommandOk {
        affected: usize,
        modified_catalog: bool,
    },
    /// `SELECT` returned a (possibly empty) row set.
    Rows {
        columns: Vec<ColumnSchema>,
        rows: Vec<Row>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineError {
    Parse(ParseError),
    Storage(StorageError),
    Eval(EvalError),
    /// Front-end accepted a construct that the v0.x executor doesn't support.
    Unsupported(String),
    /// `BEGIN` while another transaction is already open.
    TransactionAlreadyOpen,
    /// `COMMIT` / `ROLLBACK` with no active transaction.
    NoActiveTransaction,
    /// v4.0 sentinel: `execute_readonly` got a statement that
    /// mutates engine state (INSERT / CREATE / BEGIN / COMMIT / …).
    /// The caller should retake the write lock and dispatch through
    /// `execute(&mut self)` instead.
    WriteRequired,
    /// v4.2: a SELECT would have returned more rows than the
    /// configured `max_query_rows` cap. Carries the cap.
    RowLimitExceeded(usize),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::Storage(e) => write!(f, "storage: {e}"),
            Self::Eval(e) => write!(f, "eval: {e}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::TransactionAlreadyOpen => f.write_str("a transaction is already open"),
            Self::NoActiveTransaction => f.write_str("no active transaction"),
            Self::WriteRequired => {
                f.write_str("statement requires a write lock (use execute, not execute_readonly)")
            }
            Self::RowLimitExceeded(n) => {
                write!(f, "query exceeded max_query_rows={n}")
            }
        }
    }
}

impl From<ParseError> for EngineError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}
impl From<StorageError> for EngineError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<EvalError> for EngineError {
    fn from(e: EvalError) -> Self {
        Self::Eval(e)
    }
}

/// The execution engine. Holds the catalog and (later) other server-scope
/// state. `Engine::new()` is intentionally cheap so callers can construct one
/// per database, per test.
/// Function pointer that returns "now" as microseconds since Unix
/// epoch. The engine is `no_std`, so it can't reach for `std::time`
/// itself — callers (`spg-server`, the sqllogictest runner) inject a
/// concrete implementation. `None` means `NOW()` / `CURRENT_*` raise
/// `Unsupported`.
pub type ClockFn = fn() -> i64;

/// Function pointer that produces 16 cryptographically random bytes.
/// Like `ClockFn`, the engine is `no_std` and can't reach for /dev/urandom
/// itself — host (`spg-server`) injects an OS-backed source. `None`
/// means SQL-driven `CREATE USER` falls back to a deterministic salt
/// derived from the username (acceptable in tests; the server always
/// installs a real RNG so production paths never see this).
pub type SaltFn = fn() -> [u8; 16];

// ---- snapshot envelope (v4.1) ----
//
// Wraps a catalog blob + a user blob behind a small header so the
// server can persist both atomically without inventing a new file.
// Bare catalog blobs (v3.x) still load via `restore_envelope` since
// the magic check fails fast and the function falls back to
// `Catalog::deserialize`.
//
// Layout:
//   [8 bytes magic "SPGENV01"]
//   [u8 version = 1]
//   [u32 catalog_len][catalog bytes]
//   [u32 users_len][users bytes]

const ENVELOPE_MAGIC: &[u8; 8] = b"SPGENV01";
const ENVELOPE_VERSION: u8 = 1;

fn build_envelope(catalog: &[u8], users: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 1 + 4 + catalog.len() + 4 + users.len());
    out.extend_from_slice(ENVELOPE_MAGIC);
    out.push(ENVELOPE_VERSION);
    out.extend_from_slice(
        &u32::try_from(catalog.len())
            .expect("≤ 4G catalog")
            .to_le_bytes(),
    );
    out.extend_from_slice(catalog);
    out.extend_from_slice(
        &u32::try_from(users.len())
            .expect("≤ 4G users")
            .to_le_bytes(),
    );
    out.extend_from_slice(users);
    out
}

/// Returns `Some((catalog, users))` if the buffer is a v4.1
/// envelope; `None` if it's a bare catalog blob (v3.x compat).
fn split_envelope(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    if buf.len() < 8 + 1 + 4 || &buf[..8] != ENVELOPE_MAGIC {
        return None;
    }
    if buf[8] != ENVELOPE_VERSION {
        return None;
    }
    let mut p = 9usize;
    let cat_len = u32::from_le_bytes(buf[p..p + 4].try_into().ok()?) as usize;
    p += 4;
    if p + cat_len + 4 > buf.len() {
        return None;
    }
    let catalog = &buf[p..p + cat_len];
    p += cat_len;
    let user_len = u32::from_le_bytes(buf[p..p + 4].try_into().ok()?) as usize;
    p += 4;
    if p + user_len != buf.len() {
        return None;
    }
    let users = &buf[p..p + user_len];
    Some((catalog, users))
}

#[derive(Debug, Default)]
pub struct Engine {
    /// Committed catalog — what survives `Engine::snapshot()` and what
    /// outside-TX `SELECT`s read.
    catalog: Catalog,
    /// While `Some(_)`, all writes go into this shadow copy. `COMMIT` swaps
    /// it into `catalog`; `ROLLBACK` drops it. SELECTs during a TX read the
    /// shadow so they see uncommitted changes (own-write visibility).
    tx_catalog: Option<Catalog>,
    /// Named savepoints captured during the active transaction. Each
    /// entry holds the catalog snapshot at the moment `SAVEPOINT <name>`
    /// fired; `ROLLBACK TO <name>` restores from the entry and pops
    /// every savepoint after it. Empty outside a TX.
    savepoints: Vec<(String, Catalog)>,
    /// Optional wall clock used to satisfy `NOW()` / `CURRENT_TIMESTAMP`
    /// / `CURRENT_DATE`. Set by the host environment.
    clock: Option<ClockFn>,
    /// v4.1 cryptographic RNG for per-user password salt. Set by the
    /// host. `None` means SQL-driven `CREATE USER` uses a
    /// deterministic fallback — see `SaltFn`.
    salt_fn: Option<SaltFn>,
    /// v4.2 per-query row cap. `None` = unlimited. When set, a
    /// SELECT that materialises more than `n` rows returns
    /// `EngineError::RowLimitExceeded`. Enforced before the result
    /// is shaped into wire frames so a runaway scan can't blow the
    /// server's heap.
    max_query_rows: Option<usize>,
    /// v4.1 RBAC user table. Empty means "no RBAC configured yet" —
    /// the server decides what that means at the auth boundary
    /// (open mode vs legacy single-password mode). User CRUD goes
    /// through `create_user`/`drop_user`/`verify_user`; persistence
    /// rides the snapshot envelope alongside the catalog.
    users: UserStore,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            catalog: Catalog::new(),
            tx_catalog: None,
            savepoints: Vec::new(),
            clock: None,
            salt_fn: None,
            max_query_rows: None,
            users: UserStore::new(),
        }
    }

    /// Construct an engine restored from a previously-snapshotted catalog
    /// (see `snapshot()`).
    pub fn restore(catalog: Catalog) -> Self {
        Self {
            catalog,
            tx_catalog: None,
            savepoints: Vec::new(),
            clock: None,
            salt_fn: None,
            max_query_rows: None,
            users: UserStore::new(),
        }
    }

    /// Restore an engine + user table from a v4.1 envelope produced
    /// by `snapshot_with_users()`. Falls back to plain catalog-only
    /// restore if the envelope magic isn't present (so v3.x snapshot
    /// files still load).
    pub fn restore_envelope(buf: &[u8]) -> Result<Self, EngineError> {
        if let Some((catalog_bytes, user_bytes)) = split_envelope(buf) {
            let catalog = Catalog::deserialize(catalog_bytes).map_err(EngineError::Storage)?;
            let users = users::deserialize_users(user_bytes)
                .map_err(|e| EngineError::Unsupported(alloc::format!("users restore: {e}")))?;
            Ok(Self {
                catalog,
                tx_catalog: None,
                savepoints: Vec::new(),
                clock: None,
                salt_fn: None,
                max_query_rows: None,
                users,
            })
        } else {
            let catalog = Catalog::deserialize(buf).map_err(EngineError::Storage)?;
            Ok(Self::restore(catalog))
        }
    }

    pub const fn users(&self) -> &UserStore {
        &self.users
    }

    /// `salt` is supplied by the caller (the host has a random
    /// source; the engine is `no_std`). Caller should pass a fresh
    /// 16-byte random value per user.
    pub fn create_user(
        &mut self,
        name: &str,
        password: &str,
        role: Role,
        salt: [u8; 16],
    ) -> Result<(), UserError> {
        self.users.create(name, password, role, salt)
    }

    pub fn drop_user(&mut self, name: &str) -> Result<(), UserError> {
        self.users.drop(name)
    }

    pub fn verify_user(&self, name: &str, password: &str) -> Option<Role> {
        self.users.verify(name, password)
    }

    /// Builder: attach a wall clock so `NOW()` / `CURRENT_TIMESTAMP` /
    /// `CURRENT_DATE` evaluate to a real value instead of erroring out.
    #[must_use]
    pub const fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Builder: attach an OS-backed RNG for per-user password salts.
    /// The host (`spg-server`) typically wires this to `/dev/urandom`.
    #[must_use]
    pub const fn with_salt_fn(mut self, f: SaltFn) -> Self {
        self.salt_fn = Some(f);
        self
    }

    /// Builder: cap the number of rows a single SELECT may return.
    /// Exceeding the cap raises `EngineError::RowLimitExceeded` —
    /// the bound is checked inside the executor so a runaway
    /// catalog scan can't allocate millions of rows before the
    /// server gets a chance to reject the result.
    #[must_use]
    pub const fn with_max_query_rows(mut self, n: usize) -> Self {
        self.max_query_rows = Some(n);
        self
    }

    /// The *committed* catalog. Note: during a transaction this returns the
    /// pre-TX state — `SELECT` inside a TX goes through `execute()` and reads
    /// the shadow. Tests that inspect outside-TX state should use this.
    pub const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Serialize the *committed* catalog to bytes. v0.6 was full-snapshot; v0.9
    /// adds the rule that an open TX's shadow is never snapshotted — only the
    /// post-COMMIT state is persisted. v4.1 wraps the catalog in an envelope
    /// when there are users to persist; an empty user table snapshots as the
    /// bare catalog format (backwards-compat with v3.x readers).
    pub fn snapshot(&self) -> Vec<u8> {
        if self.users.is_empty() {
            self.catalog.serialize()
        } else {
            build_envelope(
                &self.catalog.serialize(),
                &users::serialize_users(&self.users),
            )
        }
    }

    pub const fn in_transaction(&self) -> bool {
        self.tx_catalog.is_some()
    }

    fn active_catalog(&self) -> &Catalog {
        self.tx_catalog.as_ref().unwrap_or(&self.catalog)
    }

    fn active_catalog_mut(&mut self) -> &mut Catalog {
        if let Some(tx) = self.tx_catalog.as_mut() {
            tx
        } else {
            &mut self.catalog
        }
    }

    /// Read-only execute path. Succeeds for `SELECT` / `SHOW TABLES`
    /// / `SHOW COLUMNS`; returns `EngineError::WriteRequired` for
    /// every other statement, so the caller can fall through to the
    /// `&mut self` `execute` path under a write lock. Engine state is
    /// not mutated even on the success path (`rewrite_clock_calls`
    /// and `resolve_order_by_position` both mutate the locally-owned
    /// AST, not `self`).
    ///
    /// **v4.0 concurrency**: this is the entry point the server takes
    /// under an `RwLock::read()` so multiple `SELECT` clients run in
    /// parallel without serialising on a single mutex.
    pub fn execute_readonly(&self, sql: &str) -> Result<QueryResult, EngineError> {
        let mut stmt = parser::parse_statement(sql)?;
        let now_micros = self.clock.map(|f| f());
        rewrite_clock_calls(&mut stmt, now_micros);
        if let Statement::Select(s) = &mut stmt {
            resolve_order_by_position(s);
        }
        let result = match stmt {
            Statement::Select(s) => self.exec_select(&s),
            Statement::ShowTables => Ok(self.exec_show_tables()),
            Statement::ShowColumns(table) => self.exec_show_columns(&table),
            Statement::ShowUsers => Ok(self.exec_show_users()),
            _ => Err(EngineError::WriteRequired),
        };
        self.enforce_row_limit(result)
    }

    /// v4.2: cap result-set size. Applied after the executor
    /// materialises rows but before they leave the engine — wrapping
    /// every Rows-returning exec_* function would scatter the check.
    fn enforce_row_limit(
        &self,
        result: Result<QueryResult, EngineError>,
    ) -> Result<QueryResult, EngineError> {
        if let (Ok(QueryResult::Rows { rows, .. }), Some(cap)) = (&result, self.max_query_rows)
            && rows.len() > cap
        {
            return Err(EngineError::RowLimitExceeded(cap));
        }
        result
    }

    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        let mut stmt = parser::parse_statement(sql)?;
        // Replace `NOW()` / `CURRENT_TIMESTAMP()` / `CURRENT_DATE()`
        // function calls with the engine's clock reading, rewritten
        // as `<literal int>::TIMESTAMP` / `::DATE`. The rewrite reads
        // the clock once per statement so every reference observes the
        // same instant. The walk is structured as a single `match` so
        // expressions that don't reference the clock (the common case)
        // take exactly one pattern dispatch and bail out.
        let now_micros = self.clock.map(|f| f());
        rewrite_clock_calls(&mut stmt, now_micros);
        // `ORDER BY <n>` (1-based select-list position) gets resolved
        // to the matching SELECT item expression here so the executor
        // can treat it uniformly with all the other order_by forms.
        if let Statement::Select(s) = &mut stmt {
            resolve_order_by_position(s);
        }
        let result = match stmt {
            Statement::CreateTable(s) => self.exec_create_table(s),
            Statement::CreateIndex(s) => self.exec_create_index(s),
            Statement::Insert(s) => self.exec_insert(s),
            Statement::Update(s) => self.exec_update(&s),
            Statement::Delete(s) => self.exec_delete(&s),
            Statement::Select(s) => self.exec_select(&s),
            Statement::Begin => self.exec_begin(),
            Statement::Commit => self.exec_commit(),
            Statement::Rollback => self.exec_rollback(),
            Statement::Savepoint(name) => self.exec_savepoint(name),
            Statement::RollbackToSavepoint(name) => self.exec_rollback_to_savepoint(&name),
            Statement::ReleaseSavepoint(name) => self.exec_release_savepoint(&name),
            Statement::ShowTables => Ok(self.exec_show_tables()),
            Statement::ShowColumns(table) => self.exec_show_columns(&table),
            Statement::ShowUsers => Ok(self.exec_show_users()),
            Statement::CreateUser(s) => self.exec_create_user(&s),
            Statement::DropUser(name) => self.exec_drop_user(&name),
        };
        self.enforce_row_limit(result)
    }

    /// v4.1 `SHOW USERS` — `(name, role)` per row, ordered by name.
    fn exec_show_users(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("role", DataType::Text, false),
        ];
        let rows: Vec<Row> = self
            .users
            .iter()
            .map(|(name, rec)| {
                Row::new(alloc::vec![
                    Value::Text(name.to_string()),
                    Value::Text(rec.role.as_str().to_string()),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    fn exec_create_user(&mut self, s: &CreateUserStatement) -> Result<QueryResult, EngineError> {
        if self.in_transaction() {
            return Err(EngineError::Unsupported(
                "CREATE USER is not allowed inside a transaction".into(),
            ));
        }
        let role = users::Role::parse(&s.role).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("invalid role: {:?}", s.role))
        })?;
        // Prefer the host-injected RNG. Falls back to a deterministic
        // salt derived from the username only when no RNG is wired —
        // acceptable for tests; the server always installs one.
        let salt = self.salt_fn.map_or_else(
            || {
                let mut s_bytes = [0u8; 16];
                let digest = spg_crypto::hash(s.name.as_bytes());
                s_bytes.copy_from_slice(&digest[..16]);
                s_bytes
            },
            |f| f(),
        );
        self.users
            .create(&s.name, &s.password, role, salt)
            .map_err(|e| EngineError::Unsupported(alloc::format!("CREATE USER: {e}")))?;
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: true,
        })
    }

    fn exec_drop_user(&mut self, name: &str) -> Result<QueryResult, EngineError> {
        if self.in_transaction() {
            return Err(EngineError::Unsupported(
                "DROP USER is not allowed inside a transaction".into(),
            ));
        }
        self.users
            .drop(name)
            .map_err(|e| EngineError::Unsupported(alloc::format!("DROP USER: {e}")))?;
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: true,
        })
    }

    /// v4.4 `UPDATE <table> SET col = expr [, ...] [WHERE cond]`.
    /// Filter pass uses the same WHERE eval as `exec_select`. Per
    /// matched row, evaluate each RHS expression against the *old*
    /// row, then call `Table::update_row` which rebuilds indices.
    /// Indexed columns are correctly reflected because rebuild
    /// happens after the cell rewrite.
    fn exec_update(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
    ) -> Result<QueryResult, EngineError> {
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        // Resolve each SET target to a column position once, validate
        // up front so a typo'd column doesn't leave a partial mutation
        // behind.
        let mut targets: Vec<(usize, &Expr)> = Vec::with_capacity(stmt.assignments.len());
        for (col, expr) in &stmt.assignments {
            let pos = schema_cols
                .iter()
                .position(|c| c.name == *col)
                .ok_or_else(|| {
                    EngineError::Eval(EvalError::ColumnNotFound { name: col.clone() })
                })?;
            targets.push((pos, expr));
        }
        let ctx = EvalContext::new(&schema_cols, Some(stmt.table.as_str()));
        // Walk every row, evaluate WHERE then SET expressions. We
        // gather (position, new_values) tuples first and apply them
        // afterwards so the WHERE/RHS evaluation reads the original
        // row state — matches PG semantics (UPDATE doesn't see its
        // own writes).
        let mut planned: Vec<(usize, Vec<Value>)> = Vec::new();
        for (i, row) in table.rows().iter().enumerate() {
            if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            let mut new_vals = row.values.clone();
            for (pos, expr) in &targets {
                let v = eval::eval_expr(expr, row, &ctx)?;
                new_vals[*pos] =
                    coerce_value(v, schema_cols[*pos].ty, &schema_cols[*pos].name, *pos)?;
            }
            planned.push((i, new_vals));
        }
        let affected = planned.len();
        for (pos, vals) in planned {
            table.update_row(pos, vals)?;
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v4.4 `DELETE FROM <table> [WHERE cond]`. Collects matching
    /// positions then delegates to `Table::delete_rows` (single index
    /// rebuild for the batch).
    fn exec_delete(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
    ) -> Result<QueryResult, EngineError> {
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        let ctx = EvalContext::new(&schema_cols, Some(stmt.table.as_str()));
        let mut positions: Vec<usize> = Vec::new();
        for (i, row) in table.rows().iter().enumerate() {
            let keep = if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                !matches!(cond, Value::Bool(true))
            } else {
                false
            };
            if !keep {
                positions.push(i);
            }
        }
        let affected = table.delete_rows(&positions);
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// `SHOW TABLES` — one row per table in the active catalog.
    /// Column name is `name` so result-set consumers can downstream
    /// `SELECT name FROM ...` style logic if needed.
    fn exec_show_tables(&self) -> QueryResult {
        let columns = alloc::vec![ColumnSchema::new("name", DataType::Text, false)];
        let rows: Vec<Row> = self
            .active_catalog()
            .table_names()
            .into_iter()
            .map(|n| Row::new(alloc::vec![Value::Text(n)]))
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// `SHOW COLUMNS FROM <table>` — one row per column with the
    /// declared name, SQL type rendering, and nullability flag.
    fn exec_show_columns(&self, table_name: &str) -> Result<QueryResult, EngineError> {
        let table =
            self.active_catalog()
                .get(table_name)
                .ok_or_else(|| StorageError::TableNotFound {
                    name: table_name.into(),
                })?;
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("type", DataType::Text, false),
            ColumnSchema::new("nullable", DataType::Bool, false),
        ];
        let rows: Vec<Row> = table
            .schema()
            .columns
            .iter()
            .map(|c| {
                Row::new(alloc::vec![
                    Value::Text(c.name.clone()),
                    Value::Text(alloc::format!("{}", c.ty)),
                    Value::Bool(c.nullable),
                ])
            })
            .collect();
        Ok(QueryResult::Rows { columns, rows })
    }

    fn exec_begin(&mut self) -> Result<QueryResult, EngineError> {
        if self.tx_catalog.is_some() {
            return Err(EngineError::TransactionAlreadyOpen);
        }
        self.tx_catalog = Some(self.catalog.clone());
        self.savepoints.clear();
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_commit(&mut self) -> Result<QueryResult, EngineError> {
        let shadow = self
            .tx_catalog
            .take()
            .ok_or(EngineError::NoActiveTransaction)?;
        self.catalog = shadow;
        // All savepoints become permanent at COMMIT and the stack
        // resets for the next TX.
        self.savepoints.clear();
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    fn exec_rollback(&mut self) -> Result<QueryResult, EngineError> {
        if self.tx_catalog.take().is_none() {
            return Err(EngineError::NoActiveTransaction);
        }
        self.savepoints.clear();
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_savepoint(&mut self, name: String) -> Result<QueryResult, EngineError> {
        if self.tx_catalog.is_none() {
            return Err(EngineError::NoActiveTransaction);
        }
        // PG re-uses an existing savepoint name by dropping the older
        // entry and pushing a fresh one — match that behaviour so
        // application code can `SAVEPOINT sp; ...; SAVEPOINT sp` freely.
        self.savepoints.retain(|(n, _)| n != &name);
        let snapshot = self
            .tx_catalog
            .as_ref()
            .expect("tx_catalog checked above")
            .clone();
        self.savepoints.push((name, snapshot));
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_rollback_to_savepoint(&mut self, name: &str) -> Result<QueryResult, EngineError> {
        if self.tx_catalog.is_none() {
            return Err(EngineError::NoActiveTransaction);
        }
        let pos = self
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!("savepoint not found: {name}"))
            })?;
        // The savepoint stays on the stack (PG semantics): a later
        // `RELEASE` or further `ROLLBACK TO` is still allowed. Everything
        // after it is discarded.
        let snapshot = self.savepoints[pos].1.clone();
        self.savepoints.truncate(pos + 1);
        self.tx_catalog = Some(snapshot);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_release_savepoint(&mut self, name: &str) -> Result<QueryResult, EngineError> {
        if self.tx_catalog.is_none() {
            return Err(EngineError::NoActiveTransaction);
        }
        let pos = self
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!("savepoint not found: {name}"))
            })?;
        // RELEASE keeps the work since the savepoint, just discards the
        // bookmark plus everything nested under it.
        self.savepoints.truncate(pos);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_create_index(
        &mut self,
        stmt: CreateIndexStatement,
    ) -> Result<QueryResult, EngineError> {
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // `IF NOT EXISTS` reduces DuplicateIndex to a no-op CommandOk.
        if stmt.if_not_exists && table.indices().iter().any(|i| i.name == stmt.name) {
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        }
        match stmt.method {
            IndexMethod::BTree => table.add_index(stmt.name, &stmt.column)?,
            IndexMethod::Hnsw => {
                table.add_nsw_index(stmt.name, &stmt.column, spg_storage::NSW_DEFAULT_M)?;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn exec_create_table(
        &mut self,
        stmt: CreateTableStatement,
    ) -> Result<QueryResult, EngineError> {
        if stmt.if_not_exists && self.active_catalog().get(&stmt.name).is_some() {
            return Ok(QueryResult::CommandOk {
                affected: 0,
                modified_catalog: false,
            });
        }
        let cols = stmt
            .columns
            .into_iter()
            .map(column_def_to_schema)
            .collect::<Result<Vec<_>, _>>()?;
        self.active_catalog_mut()
            .create_table(TableSchema::new(stmt.name, cols))?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn exec_insert(&mut self, stmt: InsertStatement) -> Result<QueryResult, EngineError> {
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // v3.1.5: clone the columns vector only (not the whole
        // TableSchema — saves one String alloc for the table name).
        // We need an owned snapshot because we'll call `table.insert`
        // (mutable borrow on `table`) inside the row loop while
        // reading schema fields.
        let column_meta: Vec<ColumnSchema> = table.schema().columns.clone();
        let schema_cols_len = column_meta.len();
        // Build a permutation `tuple_pos[c] = Some(j)` meaning schema
        // column `c` is filled from the `j`-th tuple slot; `None` means
        // "fill with NULL". Validated once and reused for every row.
        let tuple_pos: Option<Vec<Option<usize>>> = match &stmt.columns {
            None => None, // 1-1 mapping, fast path
            Some(cols) => {
                let mut map = alloc::vec![None; schema_cols_len];
                for (j, name) in cols.iter().enumerate() {
                    let idx = column_meta
                        .iter()
                        .position(|c| c.name == *name)
                        .ok_or_else(|| {
                            EngineError::Eval(EvalError::ColumnNotFound { name: name.clone() })
                        })?;
                    if map[idx].is_some() {
                        return Err(EngineError::Storage(StorageError::ArityMismatch {
                            expected: schema_cols_len,
                            actual: cols.len(),
                        }));
                    }
                    map[idx] = Some(j);
                }
                // Omitted columns must either be nullable, carry a
                // DEFAULT, or be AUTO_INCREMENT. Catch NOT NULL
                // omissions up front so the WAL stays clean.
                for (i, col) in column_meta.iter().enumerate() {
                    if map[i].is_none()
                        && !col.nullable
                        && col.default.is_none()
                        && !col.auto_increment
                    {
                        return Err(EngineError::Storage(StorageError::NullInNotNull {
                            column: col.name.clone(),
                        }));
                    }
                }
                Some(map)
            }
        };
        let expected_tuple_len = stmt.columns.as_ref().map_or(schema_cols_len, Vec::len);
        let mut affected = 0usize;
        for tuple in stmt.rows {
            if tuple.len() != expected_tuple_len {
                return Err(EngineError::Storage(StorageError::ArityMismatch {
                    expected: expected_tuple_len,
                    actual: tuple.len(),
                }));
            }
            // Fast path: no column-list permutation → tuple slot j
            // maps to schema column j. We can zip schema with tuple
            // and skip the `raw_tuple` staging allocation entirely.
            let values: Vec<Value> = if let Some(map) = &tuple_pos {
                // Permuted path: still need raw_tuple to index by `map[i]`.
                let raw_tuple: Vec<Value> = tuple
                    .into_iter()
                    .map(literal_expr_to_value)
                    .collect::<Result<_, _>>()?;
                let mut out = Vec::with_capacity(schema_cols_len);
                for (i, col) in column_meta.iter().enumerate() {
                    let mut raw = match map[i] {
                        Some(j) => raw_tuple[j].clone(),
                        None => col.default.clone().unwrap_or(Value::Null),
                    };
                    if col.auto_increment && raw.is_null() {
                        let next = table.next_auto_value(i).ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "AUTO_INCREMENT applies to integer columns only (column `{}`)",
                                col.name
                            ))
                        })?;
                        raw = Value::BigInt(next);
                    }
                    out.push(coerce_value(raw, col.ty, &col.name, i)?);
                }
                out
            } else {
                // 1-1 mapping fast path: single Vec alloc, no raw_tuple.
                let mut out = Vec::with_capacity(schema_cols_len);
                for (i, (col, expr)) in column_meta.iter().zip(tuple).enumerate() {
                    let mut raw = literal_expr_to_value(expr)?;
                    if col.auto_increment && raw.is_null() {
                        let next = table.next_auto_value(i).ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "AUTO_INCREMENT applies to integer columns only (column `{}`)",
                                col.name
                            ))
                        })?;
                        raw = Value::BigInt(next);
                    }
                    out.push(coerce_value(raw, col.ty, &col.name, i)?);
                }
                out
            };
            table.insert(Row::new(values))?;
            affected += 1;
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn exec_select(&self, stmt: &SelectStatement) -> Result<QueryResult, EngineError> {
        // Single-block SELECT (no UNION peers) takes the fast path —
        // ORDER BY and LIMIT live on this same statement.
        if stmt.unions.is_empty() {
            return self.exec_bare_select(stmt);
        }
        // UNION path: clone-strip the head into a bare block (its own
        // DISTINCT and any inner ORDER BY are dropped by parser rule —
        // the wrapper SelectStatement carries them), execute, then chain
        // peers with left-associative dedup semantics.
        let mut head = stmt.clone();
        head.unions = Vec::new();
        head.order_by = None;
        head.limit = None;
        let QueryResult::Rows { columns, mut rows } = self.exec_bare_select(&head)? else {
            unreachable!("bare SELECT cannot return CommandOk")
        };
        for (kind, peer) in &stmt.unions {
            let QueryResult::Rows {
                columns: peer_cols,
                rows: peer_rows,
            } = self.exec_bare_select(peer)?
            else {
                unreachable!("bare SELECT cannot return CommandOk")
            };
            if peer_cols.len() != columns.len() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "UNION arity mismatch: head has {} columns, peer has {}",
                    columns.len(),
                    peer_cols.len()
                )));
            }
            rows.extend(peer_rows);
            if matches!(kind, UnionKind::Distinct) {
                rows = dedup_rows(rows);
            }
        }
        // ORDER BY at the top of a UNION applies to the combined result.
        // Eval against the projected schema (NOT the source table).
        if let Some(order) = &stmt.order_by {
            let synth_ctx = EvalContext::new(&columns, None);
            let mut tagged: Vec<(f64, Row)> = Vec::with_capacity(rows.len());
            for r in rows {
                let key = eval::eval_expr(&order.expr, &r, &synth_ctx)?;
                tagged.push((value_to_order_key(&key)?, r));
            }
            sort_by_key_with_direction(&mut tagged, order.desc);
            rows = tagged.into_iter().map(|(_, r)| r).collect();
        }
        apply_offset_and_limit(&mut rows, stmt.offset, stmt.limit);
        Ok(QueryResult::Rows { columns, rows })
    }

    #[allow(clippy::too_many_lines)]
    fn exec_bare_select(&self, stmt: &SelectStatement) -> Result<QueryResult, EngineError> {
        // Constant SELECT (no FROM) — evaluate each item once against an
        // empty dummy row. Useful for `SELECT 1`, `SELECT coalesce(...)`,
        // `SELECT '7'::INT`. Column references will surface as
        // ColumnNotFound on eval since the schema is empty.
        let Some(from) = &stmt.from else {
            let empty_schema: Vec<ColumnSchema> = Vec::new();
            let ctx = EvalContext::new(&empty_schema, None);
            let projection = build_projection(&stmt.items, &empty_schema, "")?;
            let dummy_row = Row::new(Vec::new());
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, &dummy_row, &ctx)?);
            }
            let columns: Vec<ColumnSchema> = projection
                .into_iter()
                .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
                .collect();
            return Ok(QueryResult::Rows {
                columns,
                rows: alloc::vec![Row::new(values)],
            });
        };
        // Multi-table FROM (one or more joined peers) goes through the
        // nested-loop join executor. Single-table FROM stays on the
        // existing scan + index-seek path.
        if !from.joins.is_empty() {
            return self.exec_joined_select(stmt, from);
        }
        let primary = &from.primary;
        let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
            StorageError::TableNotFound {
                name: primary.name.clone(),
            }
        })?;
        let schema_cols = &table.schema().columns;
        // The qualifier accepted on column refs is the alias (if any) else the
        // bare table name.
        let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
        let ctx = EvalContext::new(schema_cols, Some(alias));

        // NSW kNN planner: `ORDER BY col <-> literal LIMIT k` with no
        // WHERE and an NSW index on `col` skips the full scan. The
        // walk returns rows already in ascending-distance order, so
        // ORDER BY / LIMIT are honoured implicitly.
        if let Some(nsw_rows) = try_nsw_knn(stmt, table, schema_cols, alias) {
            return materialise_in_order(stmt, table, schema_cols, alias, &nsw_rows);
        }

        // Index seek: if WHERE is `col = literal` (or commuted) and the
        // referenced column has an index, iterate only the matching row
        // indices. Otherwise fall back to a full scan.
        let candidate_rows: Vec<usize> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek(w, schema_cols, table, alias))
            .unwrap_or_else(|| (0..table.row_count()).collect());

        // Aggregate path: filter rows first, then hand off to the
        // aggregate executor which does its own projection + ORDER BY.
        if aggregate::uses_aggregate(stmt) {
            let mut filtered: Vec<&Row> = Vec::new();
            for &i in &candidate_rows {
                let row = &table.rows()[i];
                if let Some(where_expr) = &stmt.where_ {
                    let cond = eval::eval_expr(where_expr, row, &ctx)?;
                    if !matches!(cond, Value::Bool(true)) {
                        continue;
                    }
                }
                filtered.push(row);
            }
            let mut agg = aggregate::run(stmt, &filtered, schema_cols, Some(alias))?;
            apply_offset_and_limit(&mut agg.rows, stmt.offset, stmt.limit);
            return Ok(QueryResult::Rows {
                columns: agg.columns,
                rows: agg.rows,
            });
        }

        let projection = build_projection(&stmt.items, schema_cols, alias)?;

        // Materialise the filter pass into `(order_key, projected_row)`
        // tuples. The order key is `None` when there's no ORDER BY clause.
        let mut tagged: Vec<(Option<f64>, Row)> = Vec::new();
        for &i in &candidate_rows {
            let row = &table.rows()[i];
            if let Some(where_expr) = &stmt.where_ {
                let cond = eval::eval_expr(where_expr, row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, row, &ctx)?);
            }
            let order_key = if let Some(order) = &stmt.order_by {
                let key = eval::eval_expr(&order.expr, row, &ctx)?;
                Some(value_to_order_key(&key)?)
            } else {
                None
            };
            tagged.push((order_key, Row::new(values)));
        }

        if let Some(order) = &stmt.order_by {
            // Partial-sort fast path: when LIMIT is small relative to
            // the row count, select_nth_unstable + sort just the
            // prefix is O(n + k log k) instead of O(n log n). DISTINCT
            // requires the full sort because de-dup happens after.
            let keep = if stmt.distinct {
                None
            } else {
                stmt.limit
                    .map(|l| l as usize + stmt.offset.map_or(0, |o| o as usize))
            };
            partial_sort_tagged(&mut tagged, keep, order.desc);
        }

        let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        if stmt.distinct {
            output_rows = dedup_rows(output_rows);
        }
        apply_offset_and_limit(&mut output_rows, stmt.offset, stmt.limit);

        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();

        Ok(QueryResult::Rows {
            columns,
            rows: output_rows,
        })
    }

    /// Multi-table SELECT executor (one or more JOIN peers).
    ///
    /// v1.10 builds the joined row set up-front via nested-loop joins,
    /// then runs WHERE + projection + ORDER BY against the combined
    /// rows. No index seek. Aggregates and DISTINCT still work because
    /// the executor delegates projection through the same shared paths.
    #[allow(clippy::too_many_lines)]
    fn exec_joined_select(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
    ) -> Result<QueryResult, EngineError> {
        // Resolve every table reference up front so we surface
        // TableNotFound before we start the cartesian work.
        let primary_table = self
            .active_catalog()
            .get(&from.primary.name)
            .ok_or_else(|| StorageError::TableNotFound {
                name: from.primary.name.clone(),
            })?;
        let primary_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str())
            .to_string();
        let mut joined_tables: Vec<(&Table, String, JoinKind, Option<&Expr>)> = Vec::new();
        for j in &from.joins {
            let t = self.active_catalog().get(&j.table.name).ok_or_else(|| {
                StorageError::TableNotFound {
                    name: j.table.name.clone(),
                }
            })?;
            let a = j
                .table
                .alias
                .as_deref()
                .unwrap_or(j.table.name.as_str())
                .to_string();
            joined_tables.push((t, a, j.kind, j.on.as_ref()));
        }

        // Build the combined schema: composite "alias.col" names so the
        // qualified-column resolver can find anything by exact match.
        let mut combined_schema: Vec<ColumnSchema> = Vec::new();
        for col in &primary_table.schema().columns {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{primary_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for (t, a, _, _) in &joined_tables {
            for col in &t.schema().columns {
                combined_schema.push(ColumnSchema::new(
                    alloc::format!("{a}.{}", col.name),
                    col.ty,
                    col.nullable,
                ));
            }
        }
        let ctx = EvalContext::new(&combined_schema, None);

        // Nested-loop join. Starting set: every primary row, padded with
        // (no joined columns yet).
        let mut working: Vec<Row> = primary_table.rows().to_vec();
        let mut produced_len = primary_table.schema().columns.len();
        for (t, _, kind, on) in &joined_tables {
            let right_arity = t.schema().columns.len();
            let mut next: Vec<Row> = Vec::new();
            for left in &working {
                let mut left_matched = false;
                for right in t.rows() {
                    let mut combined_vals = left.values.clone();
                    combined_vals.extend(right.values.iter().cloned());
                    // Pad combined to the eventual full width so the
                    // partial schema still matches positions used by ON.
                    let combined = Row::new(combined_vals);
                    let keep = if let Some(on_expr) = on {
                        let cond = eval::eval_expr(on_expr, &combined, &ctx)?;
                        matches!(cond, Value::Bool(true))
                    } else {
                        // CROSS / comma-list: every pair survives.
                        true
                    };
                    if keep {
                        next.push(combined);
                        left_matched = true;
                    }
                }
                if !left_matched && matches!(kind, JoinKind::Left) {
                    // LEFT OUTER JOIN: emit the left row with NULLs on
                    // the right side when no peer matched.
                    let mut combined_vals = left.values.clone();
                    for _ in 0..right_arity {
                        combined_vals.push(Value::Null);
                    }
                    next.push(Row::new(combined_vals));
                }
            }
            working = next;
            produced_len += right_arity;
            debug_assert!(produced_len <= combined_schema.len());
        }

        // WHERE filter against combined rows.
        let mut filtered: Vec<Row> = Vec::new();
        for row in working {
            if let Some(where_expr) = &stmt.where_ {
                let cond = eval::eval_expr(where_expr, &row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            filtered.push(row);
        }

        // Aggregate path: handle GROUP BY / aggregate calls over the
        // joined+filtered rows.
        if aggregate::uses_aggregate(stmt) {
            let refs: Vec<&Row> = filtered.iter().collect();
            let mut agg = aggregate::run(stmt, &refs, &combined_schema, None)?;
            apply_offset_and_limit(&mut agg.rows, stmt.offset, stmt.limit);
            return Ok(QueryResult::Rows {
                columns: agg.columns,
                rows: agg.rows,
            });
        }

        let projection = build_projection(&stmt.items, &combined_schema, "")?;
        let mut tagged: Vec<(Option<f64>, Row)> = Vec::new();
        for row in &filtered {
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, row, &ctx)?);
            }
            let order_key = if let Some(order) = &stmt.order_by {
                let key = eval::eval_expr(&order.expr, row, &ctx)?;
                Some(value_to_order_key(&key)?)
            } else {
                None
            };
            tagged.push((order_key, Row::new(values)));
        }
        if let Some(order) = &stmt.order_by {
            let keep = if stmt.distinct {
                None
            } else {
                stmt.limit
                    .map(|l| l as usize + stmt.offset.map_or(0, |o| o as usize))
            };
            partial_sort_tagged(&mut tagged, keep, order.desc);
        }
        let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        if stmt.distinct {
            output_rows = dedup_rows(output_rows);
        }
        apply_offset_and_limit(&mut output_rows, stmt.offset, stmt.limit);
        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();
        Ok(QueryResult::Rows {
            columns,
            rows: output_rows,
        })
    }
}

/// One row-producing projection: an expression to evaluate, the resulting
/// column's user-visible name, its inferred type, and nullability.
#[derive(Debug, Clone)]
struct ProjectedItem {
    expr: Expr,
    output_name: String,
    ty: DataType,
    nullable: bool,
}

/// Dedupe a row set, preserving first-seen order. `Row`'s `PartialEq` is
/// structural (`Vec<Value>` ⇒ pairwise `Value` equality), which gives SQL
/// `NULL = NULL → TRUE` and `NaN = NaN → FALSE`. The first agrees with
/// the spec's "two NULLs are not distinct"; the second is a tolerated
/// quirk for v1 (no NaN literals are reachable from the SQL surface).
fn dedup_rows(rows: Vec<Row>) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::with_capacity(rows.len());
    for r in rows {
        if !out.iter().any(|seen| seen == &r) {
            out.push(r);
        }
    }
    out
}

/// Coerce a `Value` to an `f64` sort key for ORDER BY. Numbers map directly;
/// NULL sorts last (treated as `+∞`); booleans are 0.0 / 1.0; text uses lex
/// order via the byte values; vectors are not sortable.
fn value_to_order_key(v: &Value) -> Result<f64, EngineError> {
    match v {
        Value::Null => Ok(f64::INFINITY),
        Value::SmallInt(n) => Ok(f64::from(*n)),
        Value::Int(n) => Ok(f64::from(*n)),
        Value::Date(d) => Ok(f64::from(*d)),
        #[allow(clippy::cast_precision_loss)]
        Value::Timestamp(t) => Ok(*t as f64),
        #[allow(clippy::cast_precision_loss)]
        Value::Numeric { scaled, scale } => {
            // Scaled integer / 10^scale, computed via f64 for sort
            // ordering only. Precision losses here only matter for
            // ORDER BY tie-breaks well past 15 significant digits.
            // `f64::powi` lives in std; we hand-roll the loop so the
            // no_std engine crate doesn't need it.
            let mut divisor = 1.0_f64;
            for _ in 0..*scale {
                divisor *= 10.0;
            }
            Ok((*scaled as f64) / divisor)
        }
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Text(s) => {
            // Lex order by codepoints — good enough for ORDER BY name.
            // Map first 8 bytes packed into u64 as a coarse key; ties fall to
            // partial_cmp Equal. v1.x can swap in a real string comparator.
            let mut key: u64 = 0;
            for &b in s.as_bytes().iter().take(8) {
                key = (key << 8) | u64::from(b);
            }
            #[allow(clippy::cast_precision_loss)]
            Ok(key as f64)
        }
        Value::Vector(_) => Err(EngineError::Unsupported(
            "ORDER BY of a raw vector column is not meaningful — use `<->`".into(),
        )),
        Value::Interval { .. } => Err(EngineError::Unsupported(
            "ORDER BY of an INTERVAL is not supported in v2.11 \
             (months vs micros has no single canonical ordering)"
                .into(),
        )),
    }
}

/// Try to plan a WHERE clause as an equality lookup against an existing
/// index. Returns the candidate row indices on success; `None` means the
/// caller should fall back to a full scan.
///
/// v0.8 recognises a single top-level `col = literal` (in either operand
/// order). AND chains and range scans land in later milestones.
/// Look for `ORDER BY col <dist-op> literal LIMIT k` against an
/// NSW-indexed vector column. Recognised distance ops: `<->` (L2),
/// `<#>` (inner product), `<=>` (cosine). When a WHERE clause is
/// present, the planner does an "over-fetch and filter" pass — it
/// asks the graph for `k * over_fetch` candidates, evaluates WHERE
/// against each, and trims back to `k`. Returns the row indices in
/// ascending-distance order when the plan applies.
fn try_nsw_knn(
    stmt: &SelectStatement,
    table: &Table,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<Vec<usize>> {
    if stmt.distinct {
        return None;
    }
    let limit = usize::try_from(stmt.limit?).ok()?;
    if limit == 0 {
        return None;
    }
    let order = stmt.order_by.as_ref()?;
    // NSW kNN returns rows ascending by distance — DESC inverts the
    // natural order, so the planner can't handle it without a sort
    // pass. Fall back to the generic ORDER BY path.
    if order.desc {
        return None;
    }
    let Expr::Binary { lhs, op, rhs } = &order.expr else {
        return None;
    };
    let metric = match op {
        BinOp::L2Distance => spg_storage::NswMetric::L2,
        BinOp::InnerProduct => spg_storage::NswMetric::InnerProduct,
        BinOp::CosineDistance => spg_storage::NswMetric::Cosine,
        _ => return None,
    };
    // Accept both `col <op> literal` and `literal <op> col`.
    let ((Expr::Column(col), literal) | (literal, Expr::Column(col))) =
        (lhs.as_ref(), rhs.as_ref())
    else {
        return None;
    };
    if let Some(q) = &col.qualifier
        && q != table_alias
    {
        return None;
    }
    let col_pos = schema_cols.iter().position(|s| s.name == col.name)?;
    let query = literal_to_vector(literal)?;
    let idx = spg_storage::nsw_index_on(table, col_pos)?;
    if let Some(where_expr) = &stmt.where_ {
        // Over-fetch and filter. The factor (10×) is a heuristic that
        // covers typical selectivity for the corpus tests; v2.x will
        // make it configurable.
        let over_fetch = limit.saturating_mul(10).max(NSW_OVER_FETCH_FLOOR);
        let candidates = spg_storage::nsw_query(table, &idx.name, &query, over_fetch, metric);
        let ctx = EvalContext::new(schema_cols, Some(table_alias));
        let mut kept: Vec<usize> = Vec::with_capacity(limit);
        for i in candidates {
            let row = &table.rows()[i];
            let cond = eval::eval_expr(where_expr, row, &ctx).ok()?;
            if matches!(cond, Value::Bool(true)) {
                kept.push(i);
                if kept.len() >= limit {
                    break;
                }
            }
        }
        Some(kept)
    } else {
        Some(spg_storage::nsw_query(
            table, &idx.name, &query, limit, metric,
        ))
    }
}

/// Lower bound on the over-fetch pool when WHERE is present — even
/// for tiny `LIMIT 1` queries we keep enough candidates to absorb a
/// few WHERE rejections.
const NSW_OVER_FETCH_FLOOR: usize = 32;

/// Pull a `Vec<f32>` out of a literal-or-cast expression. Returns
/// `None` for anything we can't fold at plan time.
fn literal_to_vector(e: &Expr) -> Option<Vec<f32>> {
    match e {
        Expr::Literal(Literal::Vector(v)) => Some(v.clone()),
        Expr::Cast { expr, .. } => literal_to_vector(expr),
        _ => None,
    }
}

/// Materialise rows in a planner-supplied order (used by the NSW path)
/// without re-running ORDER BY. The projection + LIMIT slot mirror the
/// equivalent block in `exec_bare_select`.
fn materialise_in_order(
    stmt: &SelectStatement,
    table: &Table,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    ordered_rows: &[usize],
) -> Result<QueryResult, EngineError> {
    let ctx = EvalContext::new(schema_cols, Some(table_alias));
    let projection = build_projection(&stmt.items, schema_cols, table_alias)?;
    let mut output_rows: Vec<Row> = Vec::with_capacity(ordered_rows.len());
    for &i in ordered_rows {
        let row = &table.rows()[i];
        let mut values = Vec::with_capacity(projection.len());
        for p in &projection {
            values.push(eval::eval_expr(&p.expr, row, &ctx)?);
        }
        output_rows.push(Row::new(values));
    }
    apply_offset_and_limit(&mut output_rows, stmt.offset, stmt.limit);
    let columns: Vec<ColumnSchema> = projection
        .into_iter()
        .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
        .collect();
    Ok(QueryResult::Rows {
        columns,
        rows: output_rows,
    })
}

fn try_index_seek(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
) -> Option<Vec<usize>> {
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let idx = table.index_on(col_pos)?;
    let key = IndexKey::from_value(&value)?;
    Some(idx.lookup_eq(&key).to_vec())
}

fn resolve_col_literal_pair(
    col_side: &Expr,
    lit_side: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<(usize, Value)> {
    let Expr::Column(c) = col_side else {
        return None;
    };
    if let Some(q) = &c.qualifier
        && q != table_alias
    {
        return None;
    }
    let pos = schema_cols.iter().position(|s| s.name == c.name)?;
    let Expr::Literal(l) = lit_side else {
        return None;
    };
    let v = match l {
        Literal::Integer(n) => {
            if let Ok(small) = i32::try_from(*n) {
                Value::Int(small)
            } else {
                Value::BigInt(*n)
            }
        }
        Literal::Float(x) => Value::Float(*x),
        Literal::String(s) => Value::Text(s.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        // Vector and Interval literals can't be used as B-tree index keys.
        // Tell the planner to fall back to full-scan.
        Literal::Vector(_) | Literal::Interval { .. } => return None,
    };
    Some((pos, v))
}

/// Find the schema entry that a SELECT-list `Expr::Column` refers to.
/// Mirrors `resolve_column` in `eval.rs`, but returns a proper
/// `EngineError` so the projection-build path keeps `UnknownQualifier`
/// vs `ColumnNotFound` distinct.
fn resolve_projection_column<'a>(
    c: &ColumnName,
    schema_cols: &'a [ColumnSchema],
    table_alias: &str,
) -> Result<&'a ColumnSchema, EngineError> {
    if let Some(q) = &c.qualifier {
        let composite = alloc::format!("{q}.{name}", name = c.name);
        if let Some(s) = schema_cols.iter().find(|s| s.name == composite) {
            return Ok(s);
        }
        // Single-table case: the qualifier may equal the active alias —
        // then look for the bare column name.
        if q == table_alias
            && let Some(s) = schema_cols.iter().find(|s| s.name == c.name)
        {
            return Ok(s);
        }
        // For multi-table schemas the qualifier is unknown only if no
        // column bears the "<q>." prefix. For single-table, the alias
        // mismatch alone is enough.
        let prefix = alloc::format!("{q}.");
        let qualifier_known =
            q == table_alias || schema_cols.iter().any(|s| s.name.starts_with(&prefix));
        if !qualifier_known {
            return Err(EngineError::Eval(EvalError::UnknownQualifier {
                qualifier: q.clone(),
            }));
        }
        return Err(EngineError::Eval(EvalError::ColumnNotFound {
            name: c.name.clone(),
        }));
    }
    if let Some(s) = schema_cols.iter().find(|s| s.name == c.name) {
        return Ok(s);
    }
    let suffix = alloc::format!(".{name}", name = c.name);
    let mut matches = schema_cols.iter().filter(|s| s.name.ends_with(&suffix));
    let first = matches.next();
    let extra = matches.next();
    match (first, extra) {
        (Some(s), None) => Ok(s),
        (Some(_), Some(_)) => Err(EngineError::Eval(EvalError::TypeMismatch {
            detail: alloc::format!("ambiguous column reference: {}", c.name),
        })),
        _ => Err(EngineError::Eval(EvalError::ColumnNotFound {
            name: c.name.clone(),
        })),
    }
}

fn build_projection(
    items: &[SelectItem],
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Result<Vec<ProjectedItem>, EngineError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for col in schema_cols {
                    out.push(ProjectedItem {
                        expr: Expr::Column(ColumnName {
                            qualifier: None,
                            name: col.name.clone(),
                        }),
                        output_name: col.name.clone(),
                        ty: col.ty,
                        nullable: col.nullable,
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                // Plain column ref keeps full schema info (real type +
                // nullability). Compound expressions evaluate fine but have
                // no static type — surface them as nullable TEXT, which is
                // what most clients render anyway.
                if let Expr::Column(c) = expr {
                    let sch = resolve_projection_column(c, schema_cols, table_alias)?;
                    let output_name = alias.clone().unwrap_or_else(|| c.name.clone());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: sch.ty,
                        nullable: sch.nullable,
                    });
                } else {
                    let output_name = alias.clone().unwrap_or_else(|| expr.to_string());
                    out.push(ProjectedItem {
                        expr: expr.clone(),
                        output_name,
                        ty: DataType::Text,
                        nullable: true,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Promote an integer to a NUMERIC value at the requested scale.
/// Rejects values that, after scaling, would overflow the column's
/// precision budget.
fn numeric_from_integer(
    n: i128,
    precision: u8,
    scale: u8,
    col_name: &str,
) -> Result<Value, EngineError> {
    let factor = pow10_i128(scale);
    let scaled = n.checked_mul(factor).ok_or_else(|| {
        EngineError::Unsupported(alloc::format!(
            "integer overflow scaling value for column `{col_name}` to scale {scale}"
        ))
    })?;
    check_precision(scaled, precision, col_name)?;
    Ok(Value::Numeric { scaled, scale })
}

/// Float → NUMERIC. Uses round-half-away-from-zero on `x * 10^scale`,
/// then verifies the result fits the column's precision.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn numeric_from_float(
    x: f64,
    precision: u8,
    scale: u8,
    col_name: &str,
) -> Result<Value, EngineError> {
    if !x.is_finite() {
        return Err(EngineError::Unsupported(alloc::format!(
            "cannot store non-finite float in NUMERIC column `{col_name}`"
        )));
    }
    let mut factor = 1.0_f64;
    for _ in 0..scale {
        factor *= 10.0;
    }
    // Round half-away-from-zero by biasing then casting (`as i128`
    // truncates toward zero, so the bias + truncation gives the
    // desired rounding). `f64::floor` / `ceil` live in std; we don't
    // need them — the cast handles the truncation step.
    let shifted = x * factor;
    let biased = if shifted >= 0.0 {
        shifted + 0.5
    } else {
        shifted - 0.5
    };
    // Range-check before casting back to i128 — the cast itself is
    // saturating in Rust, which would silently truncate huge inputs.
    if !(-1e38..=1e38).contains(&biased) {
        return Err(EngineError::Unsupported(alloc::format!(
            "value {x} overflows NUMERIC range for column `{col_name}`"
        )));
    }
    let scaled = biased as i128;
    check_precision(scaled, precision, col_name)?;
    Ok(Value::Numeric { scaled, scale })
}

/// Move a Numeric value from `src_scale` to `dst_scale`. Going up
/// multiplies by 10; going down rounds half-away-from-zero.
fn numeric_rescale(
    scaled: i128,
    src_scale: u8,
    precision: u8,
    dst_scale: u8,
    col_name: &str,
) -> Result<Value, EngineError> {
    let new_scaled = if dst_scale >= src_scale {
        let bump = pow10_i128(dst_scale - src_scale);
        scaled.checked_mul(bump).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!(
                "overflow rescaling NUMERIC for column `{col_name}`"
            ))
        })?
    } else {
        let drop = pow10_i128(src_scale - dst_scale);
        let half = drop / 2;
        if scaled >= 0 {
            (scaled + half) / drop
        } else {
            (scaled - half) / drop
        }
    };
    check_precision(new_scaled, precision, col_name)?;
    Ok(Value::Numeric {
        scaled: new_scaled,
        scale: dst_scale,
    })
}

/// Drop the fractional part of a scaled integer, returning the integer
/// portion (toward zero). Used for NUMERIC → INT casts.
const fn numeric_truncate_to_integer(scaled: i128, scale: u8) -> i128 {
    if scale == 0 {
        return scaled;
    }
    let factor = pow10_i128_const(scale);
    scaled / factor
}

/// Verify a scaled NUMERIC value fits the column's declared precision.
/// `precision == 0` is the "unconstrained" form (bare `NUMERIC`); we
/// skip the check there.
fn check_precision(scaled: i128, precision: u8, col_name: &str) -> Result<(), EngineError> {
    if precision == 0 {
        return Ok(());
    }
    let limit = pow10_i128(precision);
    if scaled.unsigned_abs() >= limit.unsigned_abs() {
        return Err(EngineError::Unsupported(alloc::format!(
            "NUMERIC value exceeds precision {precision} for column `{col_name}`"
        )));
    }
    Ok(())
}

const fn pow10_i128_const(p: u8) -> i128 {
    let mut acc: i128 = 1;
    let mut i = 0;
    while i < p {
        acc *= 10;
        i += 1;
    }
    acc
}

fn pow10_i128(p: u8) -> i128 {
    pow10_i128_const(p)
}

/// Walk a parsed `Statement`, swapping any `NOW()` /
/// `CURRENT_TIMESTAMP()` / `CURRENT_DATE()` function calls for a
/// literal cast that wraps the engine's per-statement clock reading.
/// When `now_micros` is `None`, calls stay as-is and surface as
/// `unknown function` at eval time — keeps the error path explicit.
fn rewrite_clock_calls(stmt: &mut Statement, now_micros: Option<i64>) {
    let Some(now) = now_micros else {
        return;
    };
    match stmt {
        Statement::Select(s) => rewrite_select_clock(s, now),
        Statement::Insert(ins) => {
            for row in &mut ins.rows {
                for e in row {
                    rewrite_expr_clock(e, now);
                }
            }
        }
        _ => {}
    }
}

fn rewrite_select_clock(s: &mut SelectStatement, now: i64) {
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            rewrite_expr_clock(expr, now);
        }
    }
    if let Some(w) = &mut s.where_ {
        rewrite_expr_clock(w, now);
    }
    if let Some(gs) = &mut s.group_by {
        for g in gs {
            rewrite_expr_clock(g, now);
        }
    }
    if let Some(h) = &mut s.having {
        rewrite_expr_clock(h, now);
    }
    if let Some(o) = &mut s.order_by {
        rewrite_expr_clock(&mut o.expr, now);
    }
    for (_, peer) in &mut s.unions {
        rewrite_select_clock(peer, now);
    }
}

/// v3.0.3 hot path: every recursion lands in exactly one `match` arm.
/// Literal / Column-with-qualifier (the dominant cases on a typical
/// AST) take a single pattern dispatch and exit. The clock-rewrite
/// targets (zero-arg `NOW` / `CURRENT_TIMESTAMP` / `CURRENT_DATE`
/// functions, and bare `CURRENT_TIMESTAMP` / `CURRENT_DATE` column
/// refs) sit on their own arms with match guards so the fall-through
/// to the recursive arms is unambiguous.
fn rewrite_expr_clock(e: &mut Expr, now: i64) {
    // Fast-path test on the no-recursion shapes first. We can't fold
    // them into the big match below because they need to *replace* `e`
    // outright; the recursive arms below match on its sub-fields.
    if let Some(replacement) = clock_replacement_for(e, now) {
        *e = replacement;
        return;
    }
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_expr_clock(lhs, now);
            rewrite_expr_clock(rhs, now);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_expr_clock(expr, now);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_expr_clock(a, now);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_expr_clock(expr, now);
            rewrite_expr_clock(pattern, now);
        }
        Expr::Extract { source, .. } => rewrite_expr_clock(source, now),
        Expr::Literal(_) | Expr::Column(_) => {}
    }
}

/// Returns `Some(Expr)` when `e` is one of the clock-call shapes that
/// must be rewritten; otherwise `None` so the caller falls through to
/// the recursive walk. Identifies both function-call forms (`NOW()` /
/// `CURRENT_TIMESTAMP()` / `CURRENT_DATE()`) and bare-identifier forms
/// (`CURRENT_TIMESTAMP` / `CURRENT_DATE` as unqualified column refs,
/// which is how PG accepts them without parens).
fn clock_replacement_for(e: &Expr, now: i64) -> Option<Expr> {
    let (kind, name) = match e {
        Expr::FunctionCall { name, args } if args.is_empty() => (ClockSite::Fn, name.as_str()),
        Expr::Column(c) if c.qualifier.is_none() => (ClockSite::BareIdent, c.name.as_str()),
        _ => return None,
    };
    // ASCII case-insensitive name match. Limited to the three keywords
    // that actually need rewriting.
    let matched = match name.len() {
        3 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("now") => Some(true),
        12 if name.eq_ignore_ascii_case("current_date") => Some(false),
        17 if name.eq_ignore_ascii_case("current_timestamp") => Some(true),
        _ => None,
    };
    let is_timestamp = matched?;
    let payload = if is_timestamp {
        now
    } else {
        now.div_euclid(86_400_000_000)
    };
    let target = if is_timestamp {
        spg_sql::ast::CastTarget::Timestamp
    } else {
        spg_sql::ast::CastTarget::Date
    };
    Some(Expr::Cast {
        expr: alloc::boxed::Box::new(Expr::Literal(spg_sql::ast::Literal::Integer(payload))),
        target,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockSite {
    Fn,
    BareIdent,
}

/// `ORDER BY <integer>` references the N-th SELECT item (1-based).
/// Swap the integer literal for the matching item's expression so the
/// executor doesn't need a special-case branch. Recurses into UNION
/// peers because each peer keeps its own SELECT list.
fn resolve_order_by_position(s: &mut SelectStatement) {
    if let Some(order) = &mut s.order_by
        && let Expr::Literal(Literal::Integer(n)) = &order.expr
        && *n >= 1
        && let Ok(idx_one_based) = usize::try_from(*n)
    {
        let idx = idx_one_based - 1;
        if idx < s.items.len()
            && let SelectItem::Expr { expr, .. } = &s.items[idx]
        {
            order.expr = expr.clone();
        }
    }
    for (_, peer) in &mut s.unions {
        resolve_order_by_position(peer);
    }
}

/// Sort `tagged` by `f64` key, reversing the comparator under DESC.
/// Used by the UNION ORDER BY path; per-block paths inline the same
/// comparator because they already hold `&OrderBy` directly.
/// v3.1.1: partial-sort helper. When `keep` (= offset + limit) is
/// strictly less than `tagged.len()`, run `select_nth_unstable_by` to
/// partition the prefix in O(n), then sort just that prefix in O(k
/// log k). Total O(n + k log k), vs O(n log n) for a full sort. The
/// caller decides what `keep` is; passing `None` (no LIMIT) keeps the
/// full-sort behaviour.
///
/// `tagged` holds `(Option<f64>, Row)` (the SELECT path) — `None` keys
/// sort last in ascending order, mirroring NULL-sorts-last in SQL.
fn partial_sort_tagged(tagged: &mut Vec<(Option<f64>, Row)>, keep: Option<usize>, desc: bool) {
    let cmp = move |a: &(Option<f64>, Row), b: &(Option<f64>, Row)| {
        let ka = a.0.unwrap_or(f64::INFINITY);
        let kb = b.0.unwrap_or(f64::INFINITY);
        let ord = ka.partial_cmp(&kb).unwrap_or(core::cmp::Ordering::Equal);
        if desc { ord.reverse() } else { ord }
    };
    match keep {
        Some(k) if k < tagged.len() && k > 0 => {
            // Partition: every element at or before index k-1 is "≤"
            // (or "≥" under DESC) every element after it. Then sort
            // just the prefix to give the caller a proper ordering of
            // the kept rows.
            let pivot = k - 1;
            tagged.select_nth_unstable_by(pivot, cmp);
            tagged[..k].sort_by(cmp);
            tagged.truncate(k);
        }
        _ => {
            tagged.sort_by(cmp);
        }
    }
}

fn sort_by_key_with_direction(tagged: &mut [(f64, Row)], desc: bool) {
    tagged.sort_by(|a, b| {
        let cmp = a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal);
        if desc { cmp.reverse() } else { cmp }
    });
}

/// Drop the first `offset` rows then truncate to `limit`. PG / `MySQL`
/// agree: OFFSET applies *after* ORDER BY but *before* LIMIT (so
/// `LIMIT 10 OFFSET 5` keeps rows 6..=15).
fn apply_offset_and_limit(rows: &mut Vec<Row>, offset: Option<u32>, limit: Option<u32>) {
    if let Some(off) = offset {
        let off = off as usize;
        if off >= rows.len() {
            rows.clear();
        } else {
            rows.drain(..off);
        }
    }
    if let Some(n) = limit {
        rows.truncate(n as usize);
    }
}

fn column_def_to_schema(c: ColumnDef) -> Result<ColumnSchema, EngineError> {
    let ty = column_type_to_data_type(c.ty);
    let mut schema = ColumnSchema::new(c.name.clone(), ty, c.nullable);
    if let Some(default_expr) = c.default {
        // DEFAULT must be a literal expression — evaluated at CREATE TABLE
        // time against an empty row context. Any column ref / aggregate
        // surfaces as the corresponding eval error.
        let raw = literal_expr_to_value(default_expr)?;
        let coerced = coerce_value(raw, ty, &c.name, 0)?;
        schema = schema.with_default(coerced);
    }
    if c.auto_increment {
        // AUTO_INCREMENT only makes sense on integer-shaped columns.
        if !matches!(ty, DataType::SmallInt | DataType::Int | DataType::BigInt) {
            return Err(EngineError::Unsupported(alloc::format!(
                "AUTO_INCREMENT requires an integer column type, got {ty:?}"
            )));
        }
        schema = schema.with_auto_increment();
    }
    Ok(schema)
}

const fn column_type_to_data_type(t: ColumnTypeName) -> DataType {
    match t {
        ColumnTypeName::SmallInt => DataType::SmallInt,
        ColumnTypeName::Int => DataType::Int,
        ColumnTypeName::BigInt => DataType::BigInt,
        ColumnTypeName::Float => DataType::Float,
        ColumnTypeName::Text => DataType::Text,
        ColumnTypeName::Varchar(n) => DataType::Varchar(n),
        ColumnTypeName::Char(n) => DataType::Char(n),
        ColumnTypeName::Bool => DataType::Bool,
        ColumnTypeName::Vector(n) => DataType::Vector(n),
        ColumnTypeName::Numeric(precision, scale) => DataType::Numeric { precision, scale },
        ColumnTypeName::Date => DataType::Date,
        ColumnTypeName::Timestamp => DataType::Timestamp,
    }
}

/// Convert an INSERT VALUES expression to a storage Value. Supports literal
/// expressions, unary-minus over numeric literals, and pgvector-style
/// `'[..]'::vector` cast (v1.2). Anything more complex returns `Unsupported`.
fn literal_expr_to_value(expr: Expr) -> Result<Value, EngineError> {
    match expr {
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Cast { expr, target } => {
            let inner_value = literal_expr_to_value(*expr)?;
            crate::eval::cast_value(inner_value, target).map_err(EngineError::Eval)
        }
        Expr::Unary {
            op: UnOp::Neg,
            expr,
        } => match *expr {
            Expr::Literal(Literal::Integer(n)) => {
                // Fold to i32 if it fits, else BigInt. Parser emits Integer(i64)
                // — overflow on negate of i64::MIN is the one edge case.
                let neg = n.checked_neg().ok_or_else(|| {
                    EngineError::Unsupported("integer literal overflow on negation".into())
                })?;
                Ok(int_value_for(neg))
            }
            Expr::Literal(Literal::Float(x)) => Ok(Value::Float(-x)),
            other => Err(EngineError::Unsupported(alloc::format!(
                "unary minus over non-literal expression: {other:?}"
            ))),
        },
        other => Err(EngineError::Unsupported(alloc::format!(
            "non-literal INSERT value expression: {other:?}"
        ))),
    }
}

fn literal_to_value(l: Literal) -> Value {
    match l {
        Literal::Integer(n) => int_value_for(n),
        Literal::Float(x) => Value::Float(x),
        Literal::String(s) => Value::Text(s),
        Literal::Bool(b) => Value::Bool(b),
        Literal::Null => Value::Null,
        Literal::Vector(v) => Value::Vector(v),
        Literal::Interval { months, micros, .. } => Value::Interval { months, micros },
    }
}

/// Pick `Int` (`i32`) when the literal fits, else `BigInt`. `INT` vs `BIGINT`
/// columns will still enforce the right tag downstream — this is just the
/// default we synthesise from an unannotated integer literal.
fn int_value_for(n: i64) -> Value {
    if let Ok(small) = i32::try_from(n) {
        Value::Int(small)
    } else {
        Value::BigInt(n)
    }
}

/// Widen / narrow `v` to fit `expected`. Numerics permit safe widening
/// (`Int → BigInt`, `Int/BigInt → Float`) and best-effort narrowing
/// (`BigInt → Int` succeeds only when the value fits in `i32`). Everything
/// else returns `TypeMismatch` carrying the column name for caller diagnostics.
/// `NULL` is always permitted; the nullability check happens later in storage.
#[allow(clippy::too_many_lines)]
fn coerce_value(
    v: Value,
    expected: DataType,
    col_name: &str,
    position: usize,
) -> Result<Value, EngineError> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let actual = v.data_type().expect("non-null");
    if actual == expected {
        return Ok(v);
    }
    let coerced =
        match (v, expected) {
            (Value::Int(n), DataType::BigInt) => Some(Value::BigInt(i64::from(n))),
            (Value::Int(n), DataType::Float) => Some(Value::Float(f64::from(n))),
            (Value::Int(n), DataType::SmallInt) => i16::try_from(n).ok().map(Value::SmallInt),
            (Value::Int(n), DataType::Numeric { precision, scale }) => Some(numeric_from_integer(
                i128::from(n),
                precision,
                scale,
                col_name,
            )?),
            (Value::SmallInt(n), DataType::Int) => Some(Value::Int(i32::from(n))),
            (Value::SmallInt(n), DataType::BigInt) => Some(Value::BigInt(i64::from(n))),
            (Value::SmallInt(n), DataType::Float) => Some(Value::Float(f64::from(n))),
            (Value::SmallInt(n), DataType::Numeric { precision, scale }) => Some(
                numeric_from_integer(i128::from(n), precision, scale, col_name)?,
            ),
            (Value::BigInt(n), DataType::Int) => i32::try_from(n).ok().map(Value::Int),
            (Value::BigInt(n), DataType::SmallInt) => i16::try_from(n).ok().map(Value::SmallInt),
            #[allow(clippy::cast_precision_loss)]
            (Value::BigInt(n), DataType::Float) => Some(Value::Float(n as f64)),
            (Value::BigInt(n), DataType::Numeric { precision, scale }) => Some(
                numeric_from_integer(i128::from(n), precision, scale, col_name)?,
            ),
            (Value::Float(x), DataType::Numeric { precision, scale }) => {
                Some(numeric_from_float(x, precision, scale, col_name)?)
            }
            // Text → DATE / TIMESTAMP: parse canonical text forms.
            (Value::Text(s), DataType::Date) => {
                let d = eval::parse_date_literal(&s).ok_or_else(|| {
                    EngineError::Eval(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cannot parse {s:?} as DATE for column `{col_name}`"
                        ),
                    })
                })?;
                Some(Value::Date(d))
            }
            (Value::Text(s), DataType::Timestamp) => {
                let t = eval::parse_timestamp_literal(&s).ok_or_else(|| {
                    EngineError::Eval(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cannot parse {s:?} as TIMESTAMP for column `{col_name}`"
                        ),
                    })
                })?;
                Some(Value::Timestamp(t))
            }
            // DATE ↔ TIMESTAMP convertibility (DATE → midnight,
            // TIMESTAMP → day truncation).
            (Value::Date(d), DataType::Timestamp) => {
                Some(Value::Timestamp(i64::from(d) * 86_400_000_000))
            }
            (Value::Timestamp(t), DataType::Date) => {
                let days = t.div_euclid(86_400_000_000);
                i32::try_from(days).ok().map(Value::Date)
            }
            (
                Value::Numeric {
                    scaled,
                    scale: src_scale,
                },
                DataType::Numeric { precision, scale },
            ) => Some(numeric_rescale(
                scaled, src_scale, precision, scale, col_name,
            )?),
            #[allow(clippy::cast_precision_loss)]
            (Value::Numeric { scaled, scale }, DataType::Float) => {
                let mut div = 1.0_f64;
                for _ in 0..scale {
                    div *= 10.0;
                }
                Some(Value::Float((scaled as f64) / div))
            }
            (Value::Numeric { scaled, scale }, DataType::Int) => {
                let truncated = numeric_truncate_to_integer(scaled, scale);
                i32::try_from(truncated).ok().map(Value::Int)
            }
            (Value::Numeric { scaled, scale }, DataType::BigInt) => {
                let truncated = numeric_truncate_to_integer(scaled, scale);
                i64::try_from(truncated).ok().map(Value::BigInt)
            }
            (Value::Numeric { scaled, scale }, DataType::SmallInt) => {
                let truncated = numeric_truncate_to_integer(scaled, scale);
                i16::try_from(truncated).ok().map(Value::SmallInt)
            }
            // VARCHAR(n) enforces an upper bound on character count.
            (Value::Text(s), DataType::Varchar(max)) => {
                if u32::try_from(s.chars().count()).unwrap_or(u32::MAX) <= max {
                    Some(Value::Text(s))
                } else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "value for VARCHAR({max}) column `{col_name}` exceeds length: \
                     {} chars",
                        s.chars().count()
                    )));
                }
            }
            // CHAR(n) right-pads with U+0020 to exactly n chars; if the input
            // is already longer we reject (PG truncates trailing-space-only;
            // staying strict for v1).
            (Value::Text(s), DataType::Char(size)) => {
                let len = u32::try_from(s.chars().count()).unwrap_or(u32::MAX);
                if len > size {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "value for CHAR({size}) column `{col_name}` exceeds length: \
                     {len} chars"
                    )));
                }
                let need = (size - len) as usize;
                let mut padded = s;
                padded.reserve(need);
                for _ in 0..need {
                    padded.push(' ');
                }
                Some(Value::Text(padded))
            }
            _ => None,
        };
    coerced.ok_or(EngineError::Storage(StorageError::TypeMismatch {
        column: col_name.into(),
        expected,
        actual,
        position,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn unwrap_command_ok(r: &QueryResult) -> usize {
        match r {
            QueryResult::CommandOk { affected, .. } => *affected,
            QueryResult::Rows { .. } => panic!("expected CommandOk, got Rows"),
        }
    }

    #[test]
    fn create_table_registers_schema() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL, b TEXT)")
            .unwrap();
        assert_eq!(e.catalog().table_count(), 1);
        let t = e.catalog().get("foo").unwrap();
        assert_eq!(t.schema().columns.len(), 2);
        assert_eq!(t.schema().columns[0].ty, DataType::Int);
        assert!(!t.schema().columns[0].nullable);
        assert_eq!(t.schema().columns[1].ty, DataType::Text);
    }

    #[test]
    fn create_table_duplicate_errors() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT)").unwrap();
        let err = e.execute("CREATE TABLE foo (a INT)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::DuplicateTable { ref name }) if name == "foo"
        ));
    }

    #[test]
    fn insert_into_unknown_table_errors() {
        let mut e = Engine::new();
        let err = e.execute("INSERT INTO ghost VALUES (1)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { ref name }) if name == "ghost"
        ));
    }

    #[test]
    fn insert_happy_path_reports_one_affected() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
        let r = e.execute("INSERT INTO foo VALUES (42)").unwrap();
        assert_eq!(unwrap_command_ok(&r), 1);
        assert_eq!(e.catalog().get("foo").unwrap().row_count(), 1);
    }

    #[test]
    fn insert_arity_mismatch_propagates() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT, b TEXT)").unwrap();
        let err = e.execute("INSERT INTO foo VALUES (1)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::ArityMismatch { .. })
        ));
    }

    #[test]
    fn insert_negative_integer_via_unary_minus() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
        e.execute("INSERT INTO foo VALUES (-7)").unwrap();
        let rows = e.catalog().get("foo").unwrap().rows();
        assert_eq!(rows[0].values[0], Value::Int(-7));
    }

    #[test]
    fn insert_non_literal_expr_unsupported() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
        let err = e.execute("INSERT INTO foo VALUES (1 + 2)").unwrap_err();
        assert!(matches!(err, EngineError::Unsupported(_)));
    }

    #[test]
    fn select_star_returns_all_rows_in_insertion_order() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL, b TEXT NOT NULL)")
            .unwrap();
        e.execute("INSERT INTO foo VALUES (1, 'one')").unwrap();
        e.execute("INSERT INTO foo VALUES (2, 'two')").unwrap();
        e.execute("INSERT INTO foo VALUES (3, 'three')").unwrap();

        let r = e.execute("SELECT * FROM foo").unwrap();
        let QueryResult::Rows { columns, rows } = r else {
            panic!("expected Rows")
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "a");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[1].values,
            vec![Value::Int(2), Value::Text("two".into())]
        );
    }

    #[test]
    fn select_star_on_empty_table_returns_zero_rows() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT)").unwrap();
        let r = e.execute("SELECT * FROM foo").unwrap();
        match r {
            QueryResult::Rows { rows, .. } => assert!(rows.is_empty()),
            QueryResult::CommandOk { .. } => panic!("expected Rows"),
        }
    }

    // --- v0.4: WHERE + projection ------------------------------------------

    fn make_three_row_users(e: &mut Engine) {
        e.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL, score INT)")
            .unwrap();
        e.execute("INSERT INTO users VALUES (1, 'alice', 90)")
            .unwrap();
        e.execute("INSERT INTO users VALUES (2, 'bob', NULL)")
            .unwrap();
        e.execute("INSERT INTO users VALUES (3, 'cara', 70)")
            .unwrap();
    }

    fn unwrap_rows(r: QueryResult) -> (Vec<ColumnSchema>, Vec<Row>) {
        match r {
            QueryResult::Rows { columns, rows } => (columns, rows),
            QueryResult::CommandOk { .. } => panic!("expected Rows"),
        }
    }

    #[test]
    fn where_filter_passes_only_true_rows() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let r = e.execute("SELECT * FROM users WHERE id > 1").unwrap();
        let (_, rows) = unwrap_rows(r);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].values[0], Value::Int(2));
        assert_eq!(rows[1].values[0], Value::Int(3));
    }

    #[test]
    fn where_with_null_result_filters_out_row() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        // score is NULL for bob → score > 80 is NULL → row excluded
        let r = e.execute("SELECT * FROM users WHERE score > 80").unwrap();
        let (_, rows) = unwrap_rows(r);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[1], Value::Text("alice".into()));
    }

    #[test]
    fn projection_named_columns() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let r = e.execute("SELECT name, score FROM users").unwrap();
        let (cols, rows) = unwrap_rows(r);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "name");
        assert_eq!(cols[1].name, "score");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].values,
            vec![Value::Text("alice".into()), Value::Int(90)]
        );
    }

    #[test]
    fn projection_with_column_alias() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let r = e
            .execute("SELECT name AS who FROM users WHERE id = 1")
            .unwrap();
        let (cols, rows) = unwrap_rows(r);
        assert_eq!(cols[0].name, "who");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Text("alice".into()));
    }

    #[test]
    fn qualified_column_with_table_alias_resolves() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let r = e
            .execute("SELECT u.id, u.name FROM users AS u WHERE u.id < 3")
            .unwrap();
        let (cols, rows) = unwrap_rows(r);
        assert_eq!(cols.len(), 2);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn qualified_column_with_wrong_alias_errors() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let err = e.execute("SELECT x.id FROM users AS u").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Eval(EvalError::UnknownQualifier { ref qualifier }) if qualifier == "x"
        ));
    }

    #[test]
    fn select_unknown_column_errors_in_projection() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let err = e.execute("SELECT ghost FROM users").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Eval(EvalError::ColumnNotFound { ref name }) if name == "ghost"
        ));
    }

    #[test]
    fn where_unknown_column_errors() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let err = e
            .execute("SELECT * FROM users WHERE ghost = 1")
            .unwrap_err();
        assert!(matches!(
            err,
            EngineError::Eval(EvalError::ColumnNotFound { .. })
        ));
    }

    #[test]
    fn expression_projection_evaluates_and_renders() {
        // Compound expressions in the SELECT list are evaluated per row;
        // the output column is typed TEXT, name defaults to the expression.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (a INT NOT NULL)").unwrap();
        e.execute("INSERT INTO t VALUES (3)").unwrap();
        let (_, rows) = unwrap_rows(e.execute("SELECT 1 + 2 FROM t").unwrap());
        assert_eq!(rows.len(), 1);
        // The expression evaluates to integer 3; rendered as the cell value
        // (storage::Value::Int(3) since arithmetic kept ints).
        assert_eq!(rows[0].values[0], Value::Int(3));
    }

    #[test]
    fn select_unknown_table_errors() {
        let mut e = Engine::new();
        let err = e.execute("SELECT * FROM ghost").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { .. })
        ));
    }

    #[test]
    fn invalid_sql_returns_parse_error() {
        // v4.4: UPDATE is now real SQL, so use a true syntactic
        // garbage payload for the parse-error path.
        let mut e = Engine::new();
        let err = e.execute("THIS_IS_NOT_A_KEYWORD foo bar baz").unwrap_err();
        assert!(matches!(err, EngineError::Parse(_)));
    }

    // --- v0.8 CREATE INDEX + index seek ------------------------------------

    #[test]
    fn create_index_registers_on_table() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        e.execute("CREATE INDEX by_name ON users (name)").unwrap();
        let t = e.catalog().get("users").unwrap();
        assert_eq!(t.indices().len(), 1);
        assert_eq!(t.indices()[0].name, "by_name");
    }

    #[test]
    fn create_index_on_unknown_table_errors() {
        let mut e = Engine::new();
        let err = e.execute("CREATE INDEX i ON ghost (a)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { .. })
        ));
    }

    #[test]
    fn create_index_on_unknown_column_errors() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        let err = e.execute("CREATE INDEX i ON users (ghost)").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::ColumnNotFound { .. })
        ));
    }

    #[test]
    fn select_eq_uses_index_returns_same_rows_as_scan() {
        // Build two engines: one with an index, one without. Same query →
        // same row set (index is a planner optimisation, not a semantic
        // change).
        let mut without = Engine::new();
        make_three_row_users(&mut without);
        let mut with = Engine::new();
        make_three_row_users(&mut with);
        with.execute("CREATE INDEX by_id ON users (id)").unwrap();

        let q = "SELECT * FROM users WHERE id = 2";
        let (_, no_idx_rows) = unwrap_rows(without.execute(q).unwrap());
        let (_, idx_rows) = unwrap_rows(with.execute(q).unwrap());
        assert_eq!(no_idx_rows, idx_rows);
        assert_eq!(idx_rows.len(), 1);
    }

    #[test]
    fn select_eq_with_no_matching_index_value_returns_empty() {
        let mut e = Engine::new();
        make_three_row_users(&mut e);
        e.execute("CREATE INDEX by_id ON users (id)").unwrap();
        let (_, rows) = unwrap_rows(e.execute("SELECT * FROM users WHERE id = 999").unwrap());
        assert_eq!(rows.len(), 0);
    }

    // --- v0.9 transactions -------------------------------------------------

    #[test]
    fn begin_sets_in_transaction_flag() {
        let mut e = Engine::new();
        assert!(!e.in_transaction());
        e.execute("BEGIN").unwrap();
        assert!(e.in_transaction());
    }

    #[test]
    fn double_begin_errors() {
        let mut e = Engine::new();
        e.execute("BEGIN").unwrap();
        let err = e.execute("BEGIN").unwrap_err();
        assert_eq!(err, EngineError::TransactionAlreadyOpen);
    }

    #[test]
    fn commit_without_begin_errors() {
        let mut e = Engine::new();
        let err = e.execute("COMMIT").unwrap_err();
        assert_eq!(err, EngineError::NoActiveTransaction);
    }

    #[test]
    fn rollback_without_begin_errors() {
        let mut e = Engine::new();
        let err = e.execute("ROLLBACK").unwrap_err();
        assert_eq!(err, EngineError::NoActiveTransaction);
    }

    #[test]
    fn commit_applies_shadow_to_committed_catalog() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
        e.execute("BEGIN").unwrap();
        e.execute("INSERT INTO t VALUES (1)").unwrap();
        e.execute("INSERT INTO t VALUES (2)").unwrap();
        e.execute("COMMIT").unwrap();
        assert!(!e.in_transaction());
        assert_eq!(e.catalog().get("t").unwrap().row_count(), 2);
    }

    #[test]
    fn rollback_discards_shadow() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
        e.execute("BEGIN").unwrap();
        e.execute("INSERT INTO t VALUES (1)").unwrap();
        e.execute("INSERT INTO t VALUES (2)").unwrap();
        e.execute("ROLLBACK").unwrap();
        assert!(!e.in_transaction());
        assert_eq!(e.catalog().get("t").unwrap().row_count(), 0);
    }

    #[test]
    fn select_during_tx_sees_uncommitted_writes_own_session() {
        // The shadow catalog is read by SELECTs while a TX is open — the
        // session can see its own pending writes.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
        e.execute("BEGIN").unwrap();
        e.execute("INSERT INTO t VALUES (42)").unwrap();
        let (_, rows) = unwrap_rows(e.execute("SELECT * FROM t").unwrap());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Int(42));
    }

    #[test]
    fn snapshot_with_no_users_is_bare_catalog_format() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        let bytes = e.snapshot();
        assert_eq!(
            &bytes[..8],
            b"SPGDB001",
            "must be the bare v3.x catalog magic"
        );
        let e2 = Engine::restore_envelope(&bytes).unwrap();
        assert!(e2.users().is_empty());
        assert_eq!(e2.catalog().table_count(), 1);
    }

    #[test]
    fn snapshot_with_users_round_trips_both_via_envelope() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        e.create_user("alice", "pw1", Role::Admin, [9; 16]).unwrap();
        e.create_user("bob", "pw2", Role::ReadOnly, [5; 16])
            .unwrap();
        let bytes = e.snapshot();
        assert_eq!(&bytes[..8], b"SPGENV01", "must be the v4.1 envelope magic");
        let e2 = Engine::restore_envelope(&bytes).unwrap();
        assert_eq!(e2.users().len(), 2);
        assert_eq!(e2.verify_user("alice", "pw1"), Some(Role::Admin));
        assert_eq!(e2.verify_user("bob", "pw2"), Some(Role::ReadOnly));
        assert_eq!(e2.verify_user("alice", "wrong"), None);
        assert_eq!(e2.catalog().table_count(), 1);
    }

    #[test]
    fn ddl_inside_tx_also_rolled_back() {
        let mut e = Engine::new();
        e.execute("BEGIN").unwrap();
        e.execute("CREATE TABLE t (v INT)").unwrap();
        // Visible inside the TX.
        e.execute("SELECT * FROM t").unwrap();
        e.execute("ROLLBACK").unwrap();
        // Gone after rollback.
        let err = e.execute("SELECT * FROM t").unwrap_err();
        assert!(matches!(
            err,
            EngineError::Storage(StorageError::TableNotFound { .. })
        ));
    }
}
