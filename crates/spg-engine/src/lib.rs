//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

pub mod aggregate;
pub mod eval;
pub mod json;
pub mod users;

pub use crate::users::{Role, ScramSecrets, UserError, UserStore};

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{
    BinOp, ColumnDef, ColumnName, ColumnTypeName, CreateIndexStatement, CreateTableStatement,
    CreateUserStatement, Expr, FrameBound, FrameKind, FromClause, IndexMethod, InsertStatement,
    JoinKind, Literal, SelectItem, SelectStatement, Statement, UnOp, UnionKind, WindowFrame,
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
    /// v4.5: cooperative cancellation — the host (server's
    /// per-query watchdog) set the cancel flag while a long-running
    /// SELECT / UPDATE / DELETE was scanning rows. The partial work
    /// is discarded; the caller should surface this as a timeout
    /// to the client.
    Cancelled,
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
            Self::Cancelled => f.write_str("query cancelled (timeout or client request)"),
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

/// v4.5 cooperative cancellation token. A long-running SELECT /
/// UPDATE / DELETE checks `is_cancelled` at row-loop checkpoints
/// and bails with `EngineError::Cancelled`. The host
/// (`spg-server`) creates an `AtomicBool` per query, spawns a
/// watchdog thread that sets it after `SPG_QUERY_TIMEOUT_MS`,
/// and passes it via `execute_with_cancel` / `execute_readonly_with_cancel`.
///
/// `CancelToken::none()` is a no-op — used by the legacy `execute`
/// and `execute_readonly` entry points so existing callers don't
/// change.
#[derive(Debug, Clone, Copy)]
pub struct CancelToken<'a> {
    flag: Option<&'a core::sync::atomic::AtomicBool>,
}

impl<'a> CancelToken<'a> {
    #[must_use]
    pub const fn none() -> Self {
        Self { flag: None }
    }

    #[must_use]
    pub const fn from_flag(f: &'a core::sync::atomic::AtomicBool) -> Self {
        Self { flag: Some(f) }
    }

    #[must_use]
    pub fn is_cancelled(self) -> bool {
        self.flag
            .is_some_and(|f| f.load(core::sync::atomic::Ordering::Relaxed))
    }

    /// Returns `Err(Cancelled)` if the token has been tripped.
    /// Used at row-loop checkpoints to bail cooperatively without
    /// scattering raw `is_cancelled` checks across the executor.
    #[inline]
    pub fn check(self) -> Result<(), EngineError> {
        if self.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            Ok(())
        }
    }
}

// ---- snapshot envelope (v4.1, extended with CRC32 in v4.37) ----
//
// Wraps a catalog blob + a user blob behind a small header so the
// server can persist both atomically without inventing a new file.
// Bare catalog blobs (v3.x) still load via `restore_envelope` since
// the magic check fails fast and the function falls back to
// `Catalog::deserialize`.
//
// Layout — v1 (v4.1, no CRC):
//   [8 bytes magic "SPGENV01"]
//   [u8 version = 1]
//   [u32 catalog_len][catalog bytes]
//   [u32 users_len][users bytes]
//
// Layout — v2 (v4.37, CRC32 of body):
//   [8 bytes magic "SPGENV01"]
//   [u8 version = 2]
//   [u32 catalog_len][catalog bytes]
//   [u32 users_len][users bytes]
//   [u32 crc32]                      ← CRC32 of every byte before it
//                                      (magic + version + sections),
//                                      bit-flip detector for the
//                                      whole snapshot file.
//
// Writers always emit v2 from v4.37 on. Readers accept both: v1
// loads with no CRC check (pre-v4.37 snapshots stay readable
// forever per STABILITY); v2 verifies the trailing CRC and refuses
// on mismatch with a `StorageError::Corrupt`.

const ENVELOPE_MAGIC: &[u8; 8] = b"SPGENV01";
const ENVELOPE_VERSION_V1: u8 = 1;
const ENVELOPE_VERSION_V2: u8 = 2;

fn build_envelope(catalog: &[u8], users: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 1 + 4 + catalog.len() + 4 + users.len() + 4);
    out.extend_from_slice(ENVELOPE_MAGIC);
    out.push(ENVELOPE_VERSION_V2);
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
    let crc = spg_crypto::crc32::crc32(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Outcome of envelope parsing: either bare-catalog fallback, a
/// successfully split (catalog, users) pair from a v1 or v2
/// envelope, or an explicit corruption error from a v2 CRC
/// mismatch. `None` (bare-catalog fallback) preserves v3.x
/// readability; `Err` keeps the CRC contract honest for v2.
enum EnvelopeParse<'a> {
    Bare,
    Pair(&'a [u8], &'a [u8]),
    CrcMismatch { expected: u32, computed: u32 },
}

/// Returns `EnvelopeParse::Pair` for a valid v1 or v2 envelope,
/// `Bare` for a buffer that doesn't look like an envelope (v3.x
/// bare catalog fallback), and `CrcMismatch` for a v2 envelope
/// whose trailing CRC32 doesn't match the body.
fn split_envelope(buf: &[u8]) -> EnvelopeParse<'_> {
    if buf.len() < 8 + 1 + 4 || &buf[..8] != ENVELOPE_MAGIC {
        return EnvelopeParse::Bare;
    }
    let version = buf[8];
    if version != ENVELOPE_VERSION_V1 && version != ENVELOPE_VERSION_V2 {
        return EnvelopeParse::Bare;
    }
    let mut p = 9usize;
    let Some(cat_len_bytes) = buf.get(p..p + 4) else {
        return EnvelopeParse::Bare;
    };
    let Ok(cat_len_arr) = cat_len_bytes.try_into() else {
        return EnvelopeParse::Bare;
    };
    let cat_len = u32::from_le_bytes(cat_len_arr) as usize;
    p += 4;
    if p + cat_len + 4 > buf.len() {
        return EnvelopeParse::Bare;
    }
    let catalog = &buf[p..p + cat_len];
    p += cat_len;
    let Some(user_len_bytes) = buf.get(p..p + 4) else {
        return EnvelopeParse::Bare;
    };
    let Ok(user_len_arr) = user_len_bytes.try_into() else {
        return EnvelopeParse::Bare;
    };
    let user_len = u32::from_le_bytes(user_len_arr) as usize;
    p += 4;
    if p + user_len > buf.len() {
        return EnvelopeParse::Bare;
    }
    let users = &buf[p..p + user_len];
    p += user_len;
    if version == ENVELOPE_VERSION_V2 {
        if p + 4 != buf.len() {
            return EnvelopeParse::Bare;
        }
        let Ok(crc_arr) = buf[p..p + 4].try_into() else {
            return EnvelopeParse::Bare;
        };
        let expected = u32::from_le_bytes(crc_arr);
        let computed = spg_crypto::crc32::crc32(&buf[..p]);
        if expected != computed {
            return EnvelopeParse::CrcMismatch { expected, computed };
        }
    } else if p != buf.len() {
        // v1: must end exactly at the users section.
        return EnvelopeParse::Bare;
    }
    EnvelopeParse::Pair(catalog, users)
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
        match split_envelope(buf) {
            EnvelopeParse::Pair(catalog_bytes, user_bytes) => {
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
            }
            EnvelopeParse::CrcMismatch { expected, computed } => {
                Err(EngineError::Storage(StorageError::Corrupt(alloc::format!(
                    "snapshot envelope CRC32 mismatch (expected={expected:#010x}, computed={computed:#010x})"
                ))))
            }
            EnvelopeParse::Bare => {
                let catalog = Catalog::deserialize(buf).map_err(EngineError::Storage)?;
                Ok(Self::restore(catalog))
            }
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
        self.users.create(name, password, role, salt)?;
        // v4.8: also derive SCRAM-SHA-256 secrets so PG-wire SASL
        // auth can verify without re-running PBKDF2 per attempt.
        // Uses a fresh salt from the host RNG (falls back to a
        // deterministic per-username salt when no RNG is wired, same
        // as the legacy hash path).
        let scram_salt = self.salt_fn.map_or_else(
            || {
                let mut s = [0u8; users::SCRAM_SALT_LEN];
                let digest = spg_crypto::hash(name.as_bytes());
                // Use bytes 16..32 of BLAKE3 so we don't reuse the
                // exact same fallback salt as the BLAKE3 hash path.
                s.copy_from_slice(&digest[16..32]);
                s
            },
            |f| f(),
        );
        self.users
            .enable_scram(name, password, scram_salt, users::SCRAM_DEFAULT_ITERS)?;
        Ok(())
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
        self.execute_readonly_with_cancel(sql, CancelToken::none())
    }

    /// v4.5 — read path with cooperative cancellation. Token's
    /// `is_cancelled` is checked at the start (so a watchdog that
    /// already fired returns Cancelled immediately) and at row-loop
    /// checkpoints inside `exec_select`. SHOW paths are O(small) and
    /// don't bother checking.
    pub fn execute_readonly_with_cancel(
        &self,
        sql: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        let mut stmt = parser::parse_statement(sql)?;
        let now_micros = self.clock.map(|f| f());
        rewrite_clock_calls(&mut stmt, now_micros);
        if let Statement::Select(s) = &mut stmt {
            resolve_order_by_position(s);
        }
        let result = match stmt {
            Statement::Select(s) => self.exec_select_cancel(&s, cancel),
            Statement::ShowTables => Ok(self.exec_show_tables()),
            Statement::ShowColumns(table) => self.exec_show_columns(&table),
            Statement::ShowUsers => Ok(self.exec_show_users()),
            Statement::Explain(e) => self.exec_explain(&e, cancel),
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
        self.execute_with_cancel(sql, CancelToken::none())
    }

    /// v4.5 — write path with cooperative cancellation. Token is
    /// checked at entry and then by `exec_update` / `exec_delete` /
    /// `exec_select_cancel` row-loop checkpoints. INSERT, DDL, and
    /// TX-state ops complete atomically and don't honour the token —
    /// killing those mid-flight would leave the catalog half-mutated.
    pub fn execute_with_cancel(
        &mut self,
        sql: &str,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        let mut stmt = parser::parse_statement(sql)?;
        let now_micros = self.clock.map(|f| f());
        rewrite_clock_calls(&mut stmt, now_micros);
        if let Statement::Select(s) = &mut stmt {
            resolve_order_by_position(s);
        }
        let result = match stmt {
            Statement::CreateTable(s) => self.exec_create_table(s),
            Statement::CreateIndex(s) => self.exec_create_index(s),
            Statement::Insert(s) => self.exec_insert(s),
            Statement::Update(s) => self.exec_update_cancel(&s, cancel),
            Statement::Delete(s) => self.exec_delete_cancel(&s, cancel),
            Statement::Select(s) => self.exec_select_cancel(&s, cancel),
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
            Statement::Explain(e) => self.exec_explain(&e, cancel),
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
    fn exec_update_cancel(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        cancel: CancelToken<'_>,
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
            // v4.5: cooperative cancel checkpoint every 256 rows so
            // a runaway UPDATE without WHERE doesn't drag past the
            // server's query-timeout watchdog.
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
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
    fn exec_delete_cancel(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
        cancel: CancelToken<'_>,
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
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
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
    /// v4.26: `EXPLAIN [ANALYZE] <select>`. Returns a single-column
    /// `QUERY PLAN` text table — first line names the top operator
    /// (Scan / Aggregate / Window / etc.), indented children list
    /// FROM joins, WHERE filters, ORDER BY / LIMIT, projection
    /// shape, and any active index hits. `ANALYZE` execs the inner
    /// SELECT and appends actual-row + elapsed-micros annotations.
    #[allow(clippy::format_push_string)]
    fn exec_explain(
        &self,
        e: &spg_sql::ast::ExplainStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let mut lines = Vec::<String>::new();
        explain_select(&e.inner, self, 0, &mut lines);
        if e.analyze {
            let started = self.clock.map(|f| f());
            let exec = self.exec_select_cancel(&e.inner, cancel)?;
            let elapsed_micros = match (self.clock, started) {
                (Some(f), Some(s)) => Some(f().saturating_sub(s)),
                _ => None,
            };
            let row_count = if let QueryResult::Rows { rows, .. } = &exec {
                rows.len()
            } else {
                0
            };
            let mut annot = alloc::format!("Actual: rows={row_count}");
            if let Some(us) = elapsed_micros {
                annot.push_str(&alloc::format!(" elapsed={us}us"));
            }
            lines.push(annot);
        }
        let columns = alloc::vec![ColumnSchema::new("QUERY PLAN", DataType::Text, false)];
        let rows: Vec<Row> = lines
            .into_iter()
            .map(|l| Row::new(alloc::vec![Value::Text(l)]))
            .collect();
        Ok(QueryResult::Rows { columns, rows })
    }

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

    /// v4.5: SELECT with cooperative cancellation. The token is
    /// honoured between UNION peers and inside the bare-SELECT row
    /// loop; HNSW kNN graph walks and the aggregate executor don't
    /// honour it yet (deferred — those paths bound their work
    /// internally by `LIMIT k` and `GROUP BY` cardinality).
    fn exec_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        // v4.11: CTEs materialise into a temporary enriched catalog
        // *before* anything else — the body SELECT can then refer
        // to CTE names via the regular FROM-clause resolution.
        // Uncorrelated only: each CTE body runs once against the
        // current catalog, not against later CTEs' results (left-
        // to-right materialisation would relax this, but we keep
        // it simple for v4.11 MVP).
        if !stmt.ctes.is_empty() {
            return self.exec_with_ctes(stmt, cancel);
        }
        // v4.10: subqueries (uncorrelated) are resolved here, before
        // the executor sees the row loop. We clone the statement so
        // we can mutate without disturbing the caller's AST — most
        // queries pass through with no subquery nodes and the clone
        // is cheap; with subqueries the materialisation cost
        // dominates anyway.
        let mut stmt_owned;
        let stmt_ref: &SelectStatement = if expr_tree_has_subquery(stmt) {
            stmt_owned = stmt.clone();
            self.resolve_select_subqueries(&mut stmt_owned, cancel)?;
            &stmt_owned
        } else {
            stmt
        };
        if stmt_ref.unions.is_empty() {
            return self.exec_bare_select_cancel(stmt_ref, cancel);
        }
        // UNION path: clone-strip the head into a bare block (its own
        // DISTINCT and any inner ORDER BY are dropped by parser rule —
        // the wrapper SelectStatement carries them), execute, then chain
        // peers with left-associative dedup semantics.
        let mut head = stmt_ref.clone();
        head.unions = Vec::new();
        head.order_by = None;
        head.limit = None;
        let QueryResult::Rows { columns, mut rows } =
            self.exec_bare_select_cancel(&head, cancel)?
        else {
            unreachable!("bare SELECT cannot return CommandOk")
        };
        for (kind, peer) in &stmt_ref.unions {
            let QueryResult::Rows {
                columns: peer_cols,
                rows: peer_rows,
            } = self.exec_bare_select_cancel(peer, cancel)?
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
    #[allow(clippy::too_many_lines)] // huge match — splitting fragments the planner
    fn exec_bare_select_cancel(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v4.12: window-function path. When the projection contains
        // any `name(args) OVER (...)` we route to the dedicated
        // executor — partition + sort + per-row window value before
        // the regular projection.
        if select_has_window(stmt) {
            return self.exec_select_with_window(stmt, cancel);
        }
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
                    let cond = self.eval_expr_with_correlated(where_expr, row, &ctx, cancel)?;
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
        for (loop_idx, &i) in candidate_rows.iter().enumerate() {
            // v4.5: cooperative cancel checkpoint every 256 rows.
            if loop_idx.is_multiple_of(256) {
                cancel.check()?;
            }
            let row = &table.rows()[i];
            if let Some(where_expr) = &stmt.where_ {
                let cond = self.eval_expr_with_correlated(where_expr, row, &ctx, cancel)?;
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
        Value::Json(_) => Err(EngineError::Unsupported(
            "ORDER BY of a JSON value is not supported — cast the document to text first".into(),
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
/// v4.10: pre-walk the WHERE / projection / etc. of a SELECT and
/// replace every subquery node with a materialised literal. SPG
/// only supports uncorrelated subqueries — the inner SELECT does
/// not see outer-row columns, so the result is the same for every
/// outer row and can be evaluated once.
///
/// Returns the rewritten statement; the caller passes this to the
/// regular row-loop executor which no longer sees Subquery nodes
/// in its tree.
impl Engine {
    /// v4.12 window executor. Implements `ROW_NUMBER` / `RANK` /
    /// `DENSE_RANK` and the partition-aware aggregates `SUM` /
    /// `AVG` / `COUNT` / `MIN` / `MAX`. The plan is:
    /// 1. Apply the WHERE filter.
    /// 2. For each unique `WindowFunction` node in the projection,
    ///    partition + sort, compute the per-row value.
    /// 3. Append the window values as synthetic columns (`__win_N`)
    ///    to the row schema.
    /// 4. Rewrite the projection to read those columns.
    /// 5. Hand off to the regular project / ORDER BY / LIMIT pipe.
    #[allow(
        clippy::too_many_lines,
        clippy::type_complexity,
        clippy::needless_range_loop
    )] // window-eval is one cohesive pipe; splitting fragments
    fn exec_select_with_window(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let from = stmt.from.as_ref().ok_or_else(|| {
            EngineError::Unsupported("window functions require a FROM clause".into())
        })?;
        // For v4.12 we only support a single-table FROM. Joins +
        // windows is queued for v5.x.
        if !from.joins.is_empty() {
            return Err(EngineError::Unsupported(
                "JOIN with window functions not yet supported".into(),
            ));
        }
        let primary = &from.primary;
        let table = self.active_catalog().get(&primary.name).ok_or_else(|| {
            StorageError::TableNotFound {
                name: primary.name.clone(),
            }
        })?;
        let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
        let schema_cols = &table.schema().columns;
        let ctx = EvalContext::new(schema_cols, Some(alias));

        // 1) Filter pass.
        let mut filtered: Vec<&Row> = Vec::new();
        for (i, row) in table.rows().iter().enumerate() {
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            filtered.push(row);
        }
        let n_rows = filtered.len();

        // 2) Collect unique window function nodes from projection.
        let mut window_nodes: Vec<Expr> = Vec::new();
        for item in &stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                collect_window_nodes(expr, &mut window_nodes);
            }
        }

        // 3) For each window, compute per-row value.
        // Index: same order as window_nodes; for row i, win_vals[w][i].
        let mut win_vals: Vec<Vec<Value>> = Vec::with_capacity(window_nodes.len());
        for wnode in &window_nodes {
            let Expr::WindowFunction {
                name,
                args,
                partition_by,
                order_by,
                frame,
            } = wnode
            else {
                unreachable!("collect_window_nodes pushes only WindowFunction");
            };
            // Compute (partition_key, order_key, original_index) for each row.
            let mut indexed: Vec<(Vec<Value>, Vec<(Value, bool)>, usize)> =
                Vec::with_capacity(n_rows);
            for (i, row) in filtered.iter().enumerate() {
                let pkey: Vec<Value> = partition_by
                    .iter()
                    .map(|p| eval::eval_expr(p, row, &ctx))
                    .collect::<Result<_, _>>()?;
                let okey: Vec<(Value, bool)> = order_by
                    .iter()
                    .map(|(e, desc)| eval::eval_expr(e, row, &ctx).map(|v| (v, *desc)))
                    .collect::<Result<_, _>>()?;
                indexed.push((pkey, okey, i));
            }
            // Sort by (partition_key, order_key). Partition key uses
            // a stable encoded form; order key respects ASC/DESC.
            indexed.sort_by(|a, b| {
                let p_cmp = partition_key_cmp(&a.0, &b.0);
                if p_cmp != core::cmp::Ordering::Equal {
                    return p_cmp;
                }
                order_key_cmp(&a.1, &b.1)
            });
            // Per-partition compute.
            let mut out_vals: Vec<Value> = alloc::vec![Value::Null; n_rows];
            let mut p_start = 0;
            while p_start < indexed.len() {
                let mut p_end = p_start + 1;
                while p_end < indexed.len()
                    && partition_key_cmp(&indexed[p_start].0, &indexed[p_end].0)
                        == core::cmp::Ordering::Equal
                {
                    p_end += 1;
                }
                // Compute the function within this partition slice.
                compute_window_partition(
                    name,
                    args,
                    !order_by.is_empty(),
                    frame.as_ref(),
                    &indexed[p_start..p_end],
                    &filtered,
                    &ctx,
                    &mut out_vals,
                )?;
                p_start = p_end;
            }
            win_vals.push(out_vals);
        }

        // 4) Build extended schema: original columns + synthetic.
        let mut ext_cols = schema_cols.clone();
        for i in 0..window_nodes.len() {
            ext_cols.push(ColumnSchema::new(
                alloc::format!("__win_{i}"),
                DataType::Text, // type doesn't matter for projection eval
                true,
            ));
        }
        // 5) Build extended rows: each row gets its window values appended.
        let mut ext_rows: Vec<Row> = Vec::with_capacity(n_rows);
        for i in 0..n_rows {
            let mut values = filtered[i].values.clone();
            for w in 0..window_nodes.len() {
                values.push(win_vals[w][i].clone());
            }
            ext_rows.push(Row::new(values));
        }
        // 6) Rewrite the projection: WindowFunction nodes → Column(__win_N).
        let mut rewritten_items: Vec<SelectItem> = Vec::with_capacity(stmt.items.len());
        for item in &stmt.items {
            let new_item = match item {
                SelectItem::Wildcard => SelectItem::Wildcard,
                SelectItem::Expr { expr, alias } => {
                    let mut e = expr.clone();
                    rewrite_window_to_columns(&mut e, &window_nodes);
                    SelectItem::Expr {
                        expr: e,
                        alias: alias.clone(),
                    }
                }
            };
            rewritten_items.push(new_item);
        }

        // 7) Project into final rows.
        let ext_ctx = EvalContext::new(&ext_cols, Some(alias));
        let projection = build_projection(&rewritten_items, &ext_cols, alias)?;
        let mut tagged: Vec<(Option<f64>, Row)> = Vec::with_capacity(n_rows);
        for (i, row) in ext_rows.iter().enumerate() {
            if i.is_multiple_of(256) {
                cancel.check()?;
            }
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, row, &ext_ctx)?);
            }
            let order_key = if let Some(order) = &stmt.order_by {
                let mut e = order.expr.clone();
                rewrite_window_to_columns(&mut e, &window_nodes);
                let key = eval::eval_expr(&e, row, &ext_ctx)?;
                Some(value_to_order_key(&key)?)
            } else {
                None
            };
            tagged.push((order_key, Row::new(values)));
        }
        // ORDER BY + LIMIT/OFFSET on the projected rows.
        if let Some(order) = &stmt.order_by {
            tagged.sort_by(|a, b| {
                let (ka, kb) = (a.0.unwrap_or(f64::INFINITY), b.0.unwrap_or(f64::INFINITY));
                let cmp = ka.partial_cmp(&kb).unwrap_or(core::cmp::Ordering::Equal);
                if order.desc { cmp.reverse() } else { cmp }
            });
        }
        let mut out_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        apply_offset_and_limit(&mut out_rows, stmt.offset, stmt.limit);
        let final_cols: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();
        Ok(QueryResult::Rows {
            columns: final_cols,
            rows: out_rows,
        })
    }

    /// v4.11: materialise each CTE into a temp table inside a
    /// cloned catalog, then run the body SELECT against a fresh
    /// engine instance that owns the enriched catalog. The clone
    /// is moderately expensive — only paid by CTE-bearing queries.
    /// Subqueries inside CTE bodies / the main body resolve as
    /// usual; `clock_fn` is propagated so `NOW()` lines up.
    fn exec_with_ctes(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        cancel.check()?;
        let mut catalog = self.active_catalog().clone();
        for cte in &stmt.ctes {
            if catalog.get(&cte.name).is_some() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE name {:?} shadows an existing table; rename the CTE",
                    cte.name
                )));
            }
            let (columns, rows) = if cte.recursive {
                self.materialise_recursive_cte(cte, &catalog, cancel)?
            } else {
                let body_result = self.exec_select_cancel(&cte.body, cancel)?;
                let QueryResult::Rows { columns, rows } = body_result else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} body did not return rows",
                        cte.name
                    )));
                };
                (columns, rows)
            };
            // v4.22: the projection builder labels any non-column
            // expression as Text — including literal SELECT 1.
            // Promote each column's type to whatever the rows
            // actually carry so the CTE storage table accepts them.
            let inferred = infer_column_types(&columns, &rows);
            let mut columns = inferred;
            // v4.22: apply optional `WITH name(a, b, c)` overrides.
            if !cte.column_overrides.is_empty() {
                if cte.column_overrides.len() != columns.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} column list has {} names but body returns {} columns",
                        cte.name,
                        cte.column_overrides.len(),
                        columns.len()
                    )));
                }
                for (col, name) in columns.iter_mut().zip(cte.column_overrides.iter()) {
                    col.name.clone_from(name);
                }
            }
            let schema = TableSchema::new(cte.name.clone(), columns);
            catalog.create_table(schema).map_err(EngineError::Storage)?;
            let table = catalog
                .get_mut(&cte.name)
                .expect("just-created CTE table must exist");
            for row in rows {
                table.insert(row).map_err(EngineError::Storage)?;
            }
        }
        // Strip CTEs from the body before running on the temp engine
        // so we don't recurse forever.
        let mut body = stmt.clone();
        body.ctes = Vec::new();
        let mut temp = Engine::restore(catalog);
        if let Some(c) = self.clock {
            temp = temp.with_clock(c);
        }
        if let Some(f) = self.salt_fn {
            temp = temp.with_salt_fn(f);
        }
        temp.exec_select_cancel(&body, cancel)
    }

    /// v4.22: materialise a WITH RECURSIVE CTE. The body must be a
    /// UNION (or UNION ALL) of an anchor that does not reference
    /// the CTE name, and one or more recursive terms that do. The
    /// anchor runs first; each subsequent iteration runs the
    /// recursive term against a temp catalog where the CTE name is
    /// bound to the *previous* iteration's output. Iteration stops
    /// when the recursive term yields no rows; UNION (DISTINCT)
    /// deduplicates against the accumulated result, UNION ALL does
    /// not. A hard cap on total rows prevents runaway queries.
    #[allow(clippy::too_many_lines)]
    fn materialise_recursive_cte(
        &self,
        cte: &spg_sql::ast::Cte,
        base_catalog: &Catalog,
        cancel: CancelToken<'_>,
    ) -> Result<(Vec<ColumnSchema>, Vec<Row>), EngineError> {
        const MAX_TOTAL_ROWS: usize = 1_000_000;
        const MAX_ITERATIONS: usize = 100_000;
        cancel.check()?;
        if cte.body.unions.is_empty() {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?} body must be a UNION of an anchor and a recursive term",
                cte.name
            )));
        }
        // Anchor: the body's leading SELECT, with unions stripped.
        let mut anchor = cte.body.clone();
        let union_terms = core::mem::take(&mut anchor.unions);
        anchor.ctes = Vec::new();
        // Anchor must not reference the CTE name.
        if select_refers_to(&anchor, &cte.name) {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?}: the anchor must not reference the CTE itself",
                cte.name
            )));
        }
        let anchor_result = self.exec_select_cancel(&anchor, cancel)?;
        let QueryResult::Rows {
            columns: anchor_cols,
            rows: anchor_rows,
        } = anchor_result
        else {
            return Err(EngineError::Unsupported(alloc::format!(
                "WITH RECURSIVE {:?}: anchor did not return rows",
                cte.name
            )));
        };
        // The projection builder labels non-column expressions Text;
        // refine column types from the anchor's actual values so the
        // intermediate iter-catalog tables accept them.
        let mut columns = infer_column_types(&anchor_cols, &anchor_rows);
        if !cte.column_overrides.is_empty() {
            if cte.column_overrides.len() != columns.len() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE {:?} column list has {} names but anchor returns {} columns",
                    cte.name,
                    cte.column_overrides.len(),
                    columns.len()
                )));
            }
            for (col, name) in columns.iter_mut().zip(cte.column_overrides.iter()) {
                col.name.clone_from(name);
            }
        }
        let mut all_rows: Vec<Row> = anchor_rows.clone();
        let mut working_set: Vec<Row> = anchor_rows;
        let mut seen: alloc::collections::BTreeSet<Vec<u8>> = alloc::collections::BTreeSet::new();
        // Track at least one "all UNION ALL" flag — if every union
        // kind is ALL we skip the dedup step (faster + matches PG).
        let all_union_all = union_terms.iter().all(|(k, _)| matches!(k, UnionKind::All));
        if !all_union_all {
            for r in &all_rows {
                seen.insert(encode_row_key(r));
            }
        }
        for iter in 0..MAX_ITERATIONS {
            cancel.check()?;
            if working_set.is_empty() {
                break;
            }
            // Build a fresh catalog: base + CTE bound to working_set.
            let mut iter_catalog = base_catalog.clone();
            let schema = TableSchema::new(cte.name.clone(), columns.clone());
            iter_catalog
                .create_table(schema)
                .map_err(EngineError::Storage)?;
            {
                let table = iter_catalog.get_mut(&cte.name).expect("just-created");
                for row in &working_set {
                    table.insert(row.clone()).map_err(EngineError::Storage)?;
                }
            }
            let mut iter_engine = Engine::restore(iter_catalog);
            if let Some(c) = self.clock {
                iter_engine = iter_engine.with_clock(c);
            }
            if let Some(f) = self.salt_fn {
                iter_engine = iter_engine.with_salt_fn(f);
            }
            // Run each recursive term in sequence and collect new rows.
            let mut next_set: Vec<Row> = Vec::new();
            for (_, term) in &union_terms {
                let mut term = term.clone();
                term.ctes = Vec::new();
                let r = iter_engine.exec_select_cancel(&term, cancel)?;
                let QueryResult::Rows {
                    columns: rc,
                    rows: rs,
                } = r
                else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "WITH RECURSIVE {:?}: recursive term did not return rows",
                        cte.name
                    )));
                };
                if rc.len() != columns.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "WITH RECURSIVE {:?}: column count of recursive term ({}) does not match anchor ({})",
                        cte.name,
                        rc.len(),
                        columns.len()
                    )));
                }
                for row in rs {
                    if !all_union_all {
                        let key = encode_row_key(&row);
                        if !seen.insert(key) {
                            continue;
                        }
                    }
                    next_set.push(row);
                }
            }
            if next_set.is_empty() {
                break;
            }
            all_rows.extend(next_set.iter().cloned());
            working_set = next_set;
            if all_rows.len() > MAX_TOTAL_ROWS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "WITH RECURSIVE {:?}: produced more than {MAX_TOTAL_ROWS} rows — likely runaway recursion",
                    cte.name
                )));
            }
            if iter + 1 == MAX_ITERATIONS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "WITH RECURSIVE {:?}: exceeded {MAX_ITERATIONS} iterations",
                    cte.name
                )));
            }
        }
        Ok((columns, all_rows))
    }

    fn resolve_select_subqueries(
        &self,
        stmt: &mut SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        for item in &mut stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.resolve_expr_subqueries(expr, cancel)?;
            }
        }
        if let Some(w) = &mut stmt.where_ {
            self.resolve_expr_subqueries(w, cancel)?;
        }
        if let Some(gs) = &mut stmt.group_by {
            for g in gs {
                self.resolve_expr_subqueries(g, cancel)?;
            }
        }
        if let Some(h) = &mut stmt.having {
            self.resolve_expr_subqueries(h, cancel)?;
        }
        if let Some(o) = &mut stmt.order_by {
            self.resolve_expr_subqueries(&mut o.expr, cancel)?;
        }
        for (_, peer) in &mut stmt.unions {
            self.resolve_select_subqueries(peer, cancel)?;
        }
        Ok(())
    }

    #[allow(clippy::only_used_in_recursion)] // engine handle reads aren't really pure
    fn resolve_expr_subqueries(
        &self,
        e: &mut Expr,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        // Replace-on-this-node cases first.
        if let Some(replacement) = self.subquery_replacement(e, cancel)? {
            *e = replacement;
            return Ok(());
        }
        match e {
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr_subqueries(lhs, cancel)?;
                self.resolve_expr_subqueries(rhs, cancel)?;
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
            }
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    self.resolve_expr_subqueries(a, cancel)?;
                }
            }
            Expr::Like { expr, pattern, .. } => {
                self.resolve_expr_subqueries(expr, cancel)?;
                self.resolve_expr_subqueries(pattern, cancel)?;
            }
            Expr::Extract { source, .. } => self.resolve_expr_subqueries(source, cancel)?,
            // v4.12 window functions — recurse into args + ORDER BY
            // + PARTITION BY in case they carry inner subqueries.
            Expr::WindowFunction {
                args,
                partition_by,
                order_by,
                ..
            } => {
                for a in args {
                    self.resolve_expr_subqueries(a, cancel)?;
                }
                for p in partition_by {
                    self.resolve_expr_subqueries(p, cancel)?;
                }
                for (e, _) in order_by {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
            }
            // Subquery nodes are handled in subquery_replacement
            // (which returned None — defensive no-op); Literal /
            // Column are leaves.
            Expr::ScalarSubquery(_)
            | Expr::Exists { .. }
            | Expr::InSubquery { .. }
            | Expr::Literal(_)
            | Expr::Column(_) => {}
        }
        Ok(())
    }

    /// v4.23: per-row eval that handles correlated subqueries.
    /// Equivalent to `eval::eval_expr` when the expression has no
    /// subqueries; otherwise clones the expression, substitutes
    /// outer-row columns into each surviving subquery node, runs
    /// the inner SELECT, and replaces the node with the literal
    /// result. Only the WHERE-filter call sites use this path so
    /// the uncorrelated fast path is preserved everywhere else.
    fn eval_expr_with_correlated(
        &self,
        expr: &Expr,
        row: &Row,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
    ) -> Result<Value, EngineError> {
        if !expr_has_subquery(expr) {
            return eval::eval_expr(expr, row, ctx).map_err(EngineError::Eval);
        }
        let mut e = expr.clone();
        self.resolve_correlated_in_expr(&mut e, row, ctx, cancel)?;
        eval::eval_expr(&e, row, ctx).map_err(EngineError::Eval)
    }

    fn resolve_correlated_in_expr(
        &self,
        e: &mut Expr,
        row: &Row,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        match e {
            Expr::ScalarSubquery(inner) => {
                let mut s = (**inner).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows { rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "scalar subquery: inner did not return rows".into(),
                    ));
                };
                let value = match rows.as_slice() {
                    [] => Value::Null,
                    [r0] => r0.values.first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "scalar subquery returned {} rows; expected 0 or 1",
                            rows.len()
                        )));
                    }
                };
                *e = value_to_literal_expr(value)?;
            }
            Expr::Exists { subquery, negated } => {
                let mut s = (**subquery).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let exists = matches!(r, QueryResult::Rows { rows, .. } if !rows.is_empty());
                let bit = if *negated { !exists } else { exists };
                *e = Expr::Literal(Literal::Bool(bit));
            }
            Expr::InSubquery {
                expr: lhs,
                subquery,
                negated,
            } => {
                self.resolve_correlated_in_expr(lhs, row, ctx, cancel)?;
                let lhs_val = eval::eval_expr(lhs, row, ctx).map_err(EngineError::Eval)?;
                let mut s = (**subquery).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "IN-subquery: inner did not return rows".into(),
                    ));
                };
                if columns.len() != 1 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "IN-subquery must project exactly one column; got {}",
                        columns.len()
                    )));
                }
                let mut found = false;
                let mut any_null = false;
                for r0 in rows {
                    let v = r0.values.into_iter().next().unwrap_or(Value::Null);
                    if v.is_null() {
                        any_null = true;
                        continue;
                    }
                    if value_cmp(&v, &lhs_val) == core::cmp::Ordering::Equal {
                        found = true;
                        break;
                    }
                }
                let bit = if found {
                    !*negated
                } else if any_null {
                    return Err(EngineError::Unsupported(
                        "IN-subquery with NULL in result and no match: NULL semantics not yet implemented".into(),
                    ));
                } else {
                    *negated
                };
                *e = Expr::Literal(Literal::Bool(bit));
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_correlated_in_expr(lhs, row, ctx, cancel)?;
                self.resolve_correlated_in_expr(rhs, row, ctx, cancel)?;
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel)?;
            }
            Expr::Like { expr, pattern, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel)?;
                self.resolve_correlated_in_expr(pattern, row, ctx, cancel)?;
            }
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    self.resolve_correlated_in_expr(a, row, ctx, cancel)?;
                }
            }
            Expr::Extract { source, .. } => {
                self.resolve_correlated_in_expr(source, row, ctx, cancel)?;
            }
            Expr::WindowFunction { .. } | Expr::Literal(_) | Expr::Column(_) => {}
        }
        Ok(())
    }

    fn subquery_replacement(
        &self,
        e: &Expr,
        cancel: CancelToken<'_>,
    ) -> Result<Option<Expr>, EngineError> {
        match e {
            Expr::ScalarSubquery(inner) => {
                let mut s = (**inner).clone();
                // Recurse into the inner SELECT first so nested
                // subqueries materialise bottom-up.
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows { rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "scalar subquery: inner statement did not return rows".into(),
                    ));
                };
                let value = match rows.as_slice() {
                    [] => Value::Null,
                    [row] => row.values.first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "scalar subquery returned {} rows; expected 0 or 1",
                            rows.len()
                        )));
                    }
                };
                Ok(Some(value_to_literal_expr(value)?))
            }
            Expr::Exists { subquery, negated } => {
                let mut s = (**subquery).clone();
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let exists = match r {
                    QueryResult::Rows { rows, .. } => !rows.is_empty(),
                    QueryResult::CommandOk { .. } => false,
                };
                let bit = if *negated { !exists } else { exists };
                Ok(Some(Expr::Literal(Literal::Bool(bit))))
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let mut s = (**subquery).clone();
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "IN-subquery: inner statement did not return rows".into(),
                    ));
                };
                if columns.len() != 1 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "IN-subquery must project exactly one column; got {}",
                        columns.len()
                    )));
                }
                // Build the same OR-Eq chain the parse-time literal-list
                // path constructs, with each value lifted into a Literal.
                let mut acc: Option<Expr> = None;
                for row in rows {
                    let v = row.values.into_iter().next().unwrap_or(Value::Null);
                    let lit = value_to_literal_expr(v)?;
                    let cmp = Expr::Binary {
                        lhs: expr.clone(),
                        op: BinOp::Eq,
                        rhs: Box::new(lit),
                    };
                    acc = Some(match acc {
                        None => cmp,
                        Some(prev) => Expr::Binary {
                            lhs: Box::new(prev),
                            op: BinOp::Or,
                            rhs: Box::new(cmp),
                        },
                    });
                }
                let combined = acc.unwrap_or(Expr::Literal(Literal::Bool(false)));
                let final_expr = if *negated {
                    Expr::Unary {
                        op: UnOp::Not,
                        expr: Box::new(combined),
                    }
                } else {
                    combined
                };
                Ok(Some(final_expr))
            }
            _ => Ok(None),
        }
    }
}

// ---- v4.12 window-function helpers ----
// The (partition-key, order-key, original-index) tuple shape used
// across these helpers is intrinsic to the planner. Factoring it
// into a typedef adds indirection without making the code clearer,
// so several lints are allowed inline on the affected functions
// rather than module-wide.

/// v4.22: cheap structural scan for `FROM <name>` (qualified or
/// not) inside a SELECT — used to verify the anchor of a WITH
/// RECURSIVE CTE doesn't recurse into itself. Conservative: walks
/// FROM joins, subqueries, and unions.
fn select_refers_to(stmt: &SelectStatement, target: &str) -> bool {
    if let Some(from) = &stmt.from
        && from_refers_to(from, target)
    {
        return true;
    }
    for (_, peer) in &stmt.unions {
        if select_refers_to(peer, target) {
            return true;
        }
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && expr_refers_to(expr, target)
        {
            return true;
        }
    }
    if let Some(w) = &stmt.where_
        && expr_refers_to(w, target)
    {
        return true;
    }
    false
}

fn from_refers_to(from: &FromClause, target: &str) -> bool {
    if from.primary.name.eq_ignore_ascii_case(target) {
        return true;
    }
    from.joins
        .iter()
        .any(|j| j.table.name.eq_ignore_ascii_case(target))
}

fn expr_refers_to(e: &Expr, target: &str) -> bool {
    match e {
        Expr::ScalarSubquery(s) => select_refers_to(s, target),
        Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
            select_refers_to(subquery, target)
        }
        Expr::Binary { lhs, rhs, .. } => expr_refers_to(lhs, target) || expr_refers_to(rhs, target),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_refers_to(expr, target)
        }
        Expr::Like { expr, pattern, .. } => {
            expr_refers_to(expr, target) || expr_refers_to(pattern, target)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(|a| expr_refers_to(a, target)),
        Expr::Extract { source, .. } => expr_refers_to(source, target),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(|a| expr_refers_to(a, target))
                || partition_by.iter().any(|p| expr_refers_to(p, target))
                || order_by.iter().any(|(o, _)| expr_refers_to(o, target))
        }
        Expr::Literal(_) | Expr::Column(_) => false,
    }
}

/// v4.22: pick more specific column types from observed rows when
/// the projection builder defaulted to Text (the v1.x behavior for
/// non-column expressions). Lets `WITH t(n) AS (SELECT 1 ...)`
/// land an Int column in the CTE storage table rather than failing
/// the insert with "expected TEXT, got INT".
fn infer_column_types(columns: &[ColumnSchema], rows: &[Row]) -> Vec<ColumnSchema> {
    let mut out = columns.to_vec();
    for (col_idx, col) in out.iter_mut().enumerate() {
        if col.ty != DataType::Text {
            continue;
        }
        let mut inferred: Option<DataType> = None;
        let mut all_null = true;
        for row in rows {
            let Some(v) = row.values.get(col_idx) else {
                continue;
            };
            let ty = match v {
                Value::Null => continue,
                Value::SmallInt(_) => DataType::SmallInt,
                Value::Int(_) => DataType::Int,
                Value::BigInt(_) => DataType::BigInt,
                Value::Float(_) => DataType::Float,
                Value::Bool(_) => DataType::Bool,
                Value::Vector(_) => DataType::Vector(0),
                _ => DataType::Text,
            };
            all_null = false;
            inferred = Some(match inferred {
                None => ty,
                Some(prev) if prev == ty => prev,
                Some(_) => DataType::Text,
            });
        }
        if let Some(t) = inferred {
            col.ty = t;
            col.nullable = true;
        } else if all_null {
            col.nullable = true;
        }
    }
    out
}

/// v4.26: render a human-readable plan tree for `EXPLAIN <select>`.
/// Lines are pushed into `out`; `depth` controls indentation. We
/// describe the rewritten SELECT — what the executor *would* do —
/// using the engine handle to spot indexed lookups and table shapes.
#[allow(clippy::too_many_lines, clippy::format_push_string)]
fn explain_select(stmt: &SelectStatement, engine: &Engine, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    // 1) Top-level operator label.
    let top = if !stmt.ctes.is_empty() {
        if stmt.ctes.iter().any(|c| c.recursive) {
            "CTEScan (WITH RECURSIVE)"
        } else {
            "CTEScan (WITH)"
        }
    } else if !stmt.unions.is_empty() {
        "UnionScan"
    } else if select_has_window(stmt) {
        "WindowAgg"
    } else if aggregate::uses_aggregate(stmt) {
        "Aggregate"
    } else if stmt.distinct {
        "Distinct"
    } else if stmt.from.is_some() {
        "TableScan"
    } else {
        "Result"
    };
    out.push(alloc::format!("{pad}{top}"));
    let child = "  ".repeat(depth + 1);
    // 2) CTE bodies.
    for cte in &stmt.ctes {
        let head = if cte.recursive {
            alloc::format!("{child}CTE (recursive): {}", cte.name)
        } else {
            alloc::format!("{child}CTE: {}", cte.name)
        };
        out.push(head);
        explain_select(&cte.body, engine, depth + 2, out);
    }
    // 3) FROM details — primary table + joins, index hits.
    if let Some(from) = &stmt.from {
        let mut tag = alloc::format!("{child}From: {}", from.primary.name);
        if let Some(alias) = &from.primary.alias {
            tag.push_str(&alloc::format!(" AS {alias}"));
        }
        // Try to detect an index-seek opportunity on WHERE against
        // the primary table — same heuristic the executor uses.
        if let Some(w) = &stmt.where_
            && let Some(table) = engine.active_catalog().get(&from.primary.name)
        {
            let alias = from.primary.alias.as_deref().unwrap_or(&from.primary.name);
            let cols = &table.schema().columns;
            if try_index_seek(w, cols, table, alias).is_some() {
                tag.push_str(" [index seek]");
            } else {
                tag.push_str(" [full scan]");
            }
        } else {
            tag.push_str(" [full scan]");
        }
        out.push(tag);
        for j in &from.joins {
            let kind = match j.kind {
                spg_sql::ast::JoinKind::Inner => "INNER JOIN",
                spg_sql::ast::JoinKind::Left => "LEFT JOIN",
                spg_sql::ast::JoinKind::Cross => "CROSS JOIN",
            };
            let mut s = alloc::format!("{child}{kind}: {}", j.table.name);
            if let Some(alias) = &j.table.alias {
                s.push_str(&alloc::format!(" AS {alias}"));
            }
            if j.on.is_some() {
                s.push_str(" (ON …)");
            }
            out.push(s);
        }
    }
    // 4) WHERE / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET.
    if let Some(w) = &stmt.where_ {
        let mut s = alloc::format!("{child}Filter: {w}");
        if expr_has_subquery(w) {
            s.push_str(" [subquery]");
        }
        out.push(s);
    }
    if let Some(gs) = &stmt.group_by {
        let mut parts = Vec::new();
        for g in gs {
            parts.push(alloc::format!("{g}"));
        }
        out.push(alloc::format!("{child}GroupBy: {}", parts.join(", ")));
    }
    if let Some(h) = &stmt.having {
        out.push(alloc::format!("{child}Having: {h}"));
    }
    if let Some(o) = &stmt.order_by {
        let dir = if o.desc { "DESC" } else { "ASC" };
        out.push(alloc::format!("{child}OrderBy: {} {dir}", o.expr));
    }
    if let Some(lim) = stmt.limit {
        out.push(alloc::format!("{child}Limit: {lim}"));
    }
    if let Some(off) = stmt.offset {
        out.push(alloc::format!("{child}Offset: {off}"));
    }
    // 5) Projection — collapse Wildcard or render N items.
    if stmt
        .items
        .iter()
        .any(|it| matches!(it, SelectItem::Wildcard))
    {
        out.push(alloc::format!("{child}Project: *"));
    } else {
        out.push(alloc::format!(
            "{child}Project: {} item(s)",
            stmt.items.len()
        ));
    }
    // 6) Recurse into UNION peers.
    for (kind, peer) in &stmt.unions {
        let label = match kind {
            UnionKind::All => "UNION ALL",
            UnionKind::Distinct => "UNION",
        };
        out.push(alloc::format!("{child}{label}"));
        explain_select(peer, engine, depth + 2, out);
    }
}

/// v4.23: recognise the engine errors that indicate the inner
/// SELECT couldn't be evaluated in isolation because it references
/// an outer column — used by `subquery_replacement` to skip
/// materialisation and let row-eval handle it instead.
fn is_correlation_error(e: &EngineError) -> bool {
    matches!(
        e,
        EngineError::Eval(
            eval::EvalError::ColumnNotFound { .. } | eval::EvalError::UnknownQualifier { .. }
        )
    )
}

/// v4.23: walk every Expr in `stmt` and replace each Column ref
/// that targets the outer scope (qualifier matches the outer
/// table alias) with a Literal carrying the outer row's value.
/// Conservative: only qualified refs are substituted, so the user
/// must write `outer_alias.col` to reference an outer column. This
/// matches PG's lexical scoping for correlated subqueries and
/// avoids accidentally rebinding inner columns of the same name.
fn substitute_outer_columns(stmt: &mut SelectStatement, row: &Row, ctx: &EvalContext<'_>) {
    let Some(outer_alias) = ctx.table_alias else {
        return;
    };
    substitute_in_select(stmt, row, ctx, outer_alias);
}

fn substitute_in_select(
    stmt: &mut SelectStatement,
    row: &Row,
    ctx: &EvalContext<'_>,
    outer_alias: &str,
) {
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            substitute_in_expr(expr, row, ctx, outer_alias);
        }
    }
    if let Some(w) = &mut stmt.where_ {
        substitute_in_expr(w, row, ctx, outer_alias);
    }
    if let Some(gs) = &mut stmt.group_by {
        for g in gs {
            substitute_in_expr(g, row, ctx, outer_alias);
        }
    }
    if let Some(h) = &mut stmt.having {
        substitute_in_expr(h, row, ctx, outer_alias);
    }
    if let Some(o) = &mut stmt.order_by {
        substitute_in_expr(&mut o.expr, row, ctx, outer_alias);
    }
    for (_, peer) in &mut stmt.unions {
        substitute_in_select(peer, row, ctx, outer_alias);
    }
}

fn substitute_in_expr(e: &mut Expr, row: &Row, ctx: &EvalContext<'_>, outer_alias: &str) {
    if let Expr::Column(c) = e
        && let Some(qual) = &c.qualifier
        && qual.eq_ignore_ascii_case(outer_alias)
    {
        // Look up the column's index in the outer schema.
        if let Some(idx) = ctx
            .columns
            .iter()
            .position(|sc| sc.name.eq_ignore_ascii_case(&c.name))
        {
            let v = row.values.get(idx).cloned().unwrap_or(Value::Null);
            if let Ok(lit) = value_to_literal_expr(v) {
                *e = lit;
                return;
            }
        }
    }
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            substitute_in_expr(lhs, row, ctx, outer_alias);
            substitute_in_expr(rhs, row, ctx, outer_alias);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
            substitute_in_expr(pattern, row, ctx, outer_alias);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_in_expr(a, row, ctx, outer_alias);
            }
        }
        Expr::Extract { source, .. } => substitute_in_expr(source, row, ctx, outer_alias),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                substitute_in_expr(a, row, ctx, outer_alias);
            }
            for p in partition_by {
                substitute_in_expr(p, row, ctx, outer_alias);
            }
            for (o, _) in order_by {
                substitute_in_expr(o, row, ctx, outer_alias);
            }
        }
        Expr::ScalarSubquery(s) => substitute_in_select(s, row, ctx, outer_alias),
        Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
            substitute_in_select(subquery, row, ctx, outer_alias);
        }
        Expr::Literal(_) | Expr::Column(_) => {}
    }
}

/// v4.22: encode a Row to a comparable byte key for UNION-DISTINCT
/// dedup inside the recursive iteration. Crude but deterministic
/// — Debug prints embed type discriminants so NULL ≠ "" ≠ 0.
fn encode_row_key(row: &Row) -> Vec<u8> {
    let mut out = Vec::new();
    for v in &row.values {
        let s = alloc::format!("{v:?}|");
        out.extend_from_slice(s.as_bytes());
    }
    out
}

fn select_has_window(stmt: &SelectStatement) -> bool {
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && expr_has_window(expr)
        {
            return true;
        }
    }
    false
}

fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::WindowFunction { .. } => true,
        Expr::Binary { lhs, rhs, .. } => expr_has_window(lhs) || expr_has_window(rhs),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_has_window(expr)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_window),
        Expr::Like { expr, pattern, .. } => expr_has_window(expr) || expr_has_window(pattern),
        Expr::Extract { source, .. } => expr_has_window(source),
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::Literal(_)
        | Expr::Column(_) => false,
    }
}

fn collect_window_nodes(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::WindowFunction { .. } = e {
        // Deduplicate by structural equality on the expression
        // (cheap because window args + partition + order are
        // small). Without dedup we'd recompute identical windows
        // once per occurrence in the projection.
        if !out.iter().any(|x| x == e) {
            out.push(e.clone());
        }
        return;
    }
    match e {
        // Already handled by the early-return at the top.
        Expr::WindowFunction { .. } => unreachable!(),
        Expr::Binary { lhs, rhs, .. } => {
            collect_window_nodes(lhs, out);
            collect_window_nodes(rhs, out);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            collect_window_nodes(expr, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_window_nodes(a, out);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            collect_window_nodes(expr, out);
            collect_window_nodes(pattern, out);
        }
        Expr::Extract { source, .. } => collect_window_nodes(source, out),
        _ => {}
    }
}

fn rewrite_window_to_columns(e: &mut Expr, window_nodes: &[Expr]) {
    if let Expr::WindowFunction { .. } = e
        && let Some(idx) = window_nodes.iter().position(|w| w == e)
    {
        *e = Expr::Column(spg_sql::ast::ColumnName {
            qualifier: None,
            name: alloc::format!("__win_{idx}"),
        });
        return;
    }
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_window_to_columns(lhs, window_nodes);
            rewrite_window_to_columns(rhs, window_nodes);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_window_to_columns(expr, window_nodes);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_window_to_columns(a, window_nodes);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_window_to_columns(expr, window_nodes);
            rewrite_window_to_columns(pattern, window_nodes);
        }
        Expr::Extract { source, .. } => rewrite_window_to_columns(source, window_nodes),
        _ => {}
    }
}

/// Total order over partition-key tuples. NULL sorts as the
/// lowest value (matches the `<` partial order's NULL-last
/// behaviour with `INFINITY` flipped).
fn partition_key_cmp(a: &[Value], b: &[Value]) -> core::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let c = value_cmp(x, y);
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

fn order_key_cmp(a: &[(Value, bool)], b: &[(Value, bool)]) -> core::cmp::Ordering {
    for ((va, desc), (vb, _)) in a.iter().zip(b.iter()) {
        let c = value_cmp(va, vb);
        let c = if *desc { c.reverse() } else { c };
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

#[allow(clippy::match_same_arms)] // explicit arms per type document the supported pairs
fn value_cmp(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::SmallInt(x), Value::SmallInt(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        // Cross-type compare: fall back to the debug rendering —
        // same-partition is the goal, exact order is irrelevant.
        _ => alloc::format!("{a:?}").cmp(&alloc::format!("{b:?}")),
    }
}

/// Compute the window function's per-row output for one partition.
/// `slice` has (partition key, order key, original-row-index)
/// tuples already sorted by order key. `filtered_rows` is the
/// full row list indexed by original-row-index. `out_vals` is
/// the destination, also indexed by original-row-index.
#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::match_same_arms
)]
fn compute_window_partition(
    name: &str,
    args: &[Expr],
    ordered: bool,
    frame: Option<&WindowFrame>,
    slice: &[(Vec<Value>, Vec<(Value, bool)>, usize)],
    filtered_rows: &[&Row],
    ctx: &EvalContext<'_>,
    out_vals: &mut [Value],
) -> Result<(), EngineError> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "row_number" => {
            for (rank, (_, _, idx)) in slice.iter().enumerate() {
                out_vals[*idx] = Value::BigInt((rank + 1) as i64);
            }
            Ok(())
        }
        "rank" => {
            let mut prev_key: Option<&[(Value, bool)]> = None;
            let mut current_rank: i64 = 1;
            for (i, (_, okey, idx)) in slice.iter().enumerate() {
                if let Some(p) = prev_key
                    && order_key_cmp(p, okey) != core::cmp::Ordering::Equal
                {
                    current_rank = (i + 1) as i64;
                }
                if prev_key.is_none() {
                    current_rank = 1;
                }
                out_vals[*idx] = Value::BigInt(current_rank);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "dense_rank" => {
            let mut prev_key: Option<&[(Value, bool)]> = None;
            let mut current_rank: i64 = 0;
            for (_, okey, idx) in slice {
                if prev_key.is_none_or(|p| order_key_cmp(p, okey) != core::cmp::Ordering::Equal) {
                    current_rank += 1;
                }
                out_vals[*idx] = Value::BigInt(current_rank);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "sum" | "avg" | "min" | "max" | "count" | "count_star" => {
            // Pre-evaluate the function arg per row in the slice
            // (count_star has no arg).
            let arg_values: Vec<Value> = if lower == "count_star" || args.is_empty() {
                slice.iter().map(|_| Value::Null).collect()
            } else {
                slice
                    .iter()
                    .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                    .collect::<Result<_, _>>()
                    .map_err(EngineError::Eval)?
            };
            // v4.20: pick the effective frame. Explicit frame
            // overrides the implicit default (running for ordered,
            // whole-partition for unordered).
            let eff = effective_frame(frame, ordered)?;
            #[allow(clippy::needless_range_loop)]
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice);
                let mut sum: f64 = 0.0;
                let mut count: i64 = 0;
                let mut min_v: Option<f64> = None;
                let mut max_v: Option<f64> = None;
                let mut row_count: i64 = 0;
                if lo <= hi {
                    for j in lo..=hi {
                        let v = &arg_values[j];
                        match lower.as_str() {
                            "count_star" => row_count += 1,
                            "count" => {
                                if !v.is_null() {
                                    count += 1;
                                }
                            }
                            _ => {
                                if let Some(x) = value_to_f64(v) {
                                    sum += x;
                                    count += 1;
                                    min_v = Some(min_v.map_or(x, |m| m.min(x)));
                                    max_v = Some(max_v.map_or(x, |m| m.max(x)));
                                }
                            }
                        }
                    }
                }
                let value = match lower.as_str() {
                    "count_star" => Value::BigInt(row_count),
                    "count" => Value::BigInt(count),
                    "sum" => Value::Float(sum),
                    "avg" => {
                        if count == 0 {
                            Value::Null
                        } else {
                            Value::Float(sum / count as f64)
                        }
                    }
                    "min" => min_v.map_or(Value::Null, Value::Float),
                    "max" => max_v.map_or(Value::Null, Value::Float),
                    _ => unreachable!(),
                };
                let (_, _, idx) = &slice[i];
                out_vals[*idx] = value;
            }
            Ok(())
        }
        "lag" | "lead" => {
            // lag(expr [, offset [, default]])
            // lead(expr [, offset [, default]])
            if args.is_empty() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{lower}() requires at least one argument"
                )));
            }
            let offset: i64 = if args.len() >= 2 {
                let v = eval::eval_expr(&args[1], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?;
                match v {
                    Value::SmallInt(n) => i64::from(n),
                    Value::Int(n) => i64::from(n),
                    Value::BigInt(n) => n,
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "{lower}() offset must be integer"
                        )));
                    }
                }
            } else {
                1
            };
            let default: Value = if args.len() >= 3 {
                eval::eval_expr(&args[2], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?
            } else {
                Value::Null
            };
            let values: Vec<Value> = slice
                .iter()
                .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                .collect::<Result<_, _>>()
                .map_err(EngineError::Eval)?;
            let n = slice.len();
            for (i, (_, _, idx)) in slice.iter().enumerate() {
                let signed_offset = if lower == "lag" { -offset } else { offset };
                let target_signed = i64::try_from(i).unwrap_or(i64::MAX) + signed_offset;
                let v =
                    if target_signed < 0 || target_signed >= i64::try_from(n).unwrap_or(i64::MAX) {
                        default.clone()
                    } else {
                        #[allow(clippy::cast_sign_loss)]
                        {
                            values[target_signed as usize].clone()
                        }
                    };
                out_vals[*idx] = v;
            }
            Ok(())
        }
        "first_value" | "last_value" | "nth_value" => {
            if args.is_empty() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{lower}() requires at least one argument"
                )));
            }
            let values: Vec<Value> = slice
                .iter()
                .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                .collect::<Result<_, _>>()
                .map_err(EngineError::Eval)?;
            let nth: usize = if lower == "nth_value" {
                if args.len() < 2 {
                    return Err(EngineError::Unsupported(
                        "nth_value() requires (expr, n)".into(),
                    ));
                }
                let v = eval::eval_expr(&args[1], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?;
                let raw = match v {
                    Value::SmallInt(n) => i64::from(n),
                    Value::Int(n) => i64::from(n),
                    Value::BigInt(n) => n,
                    _ => {
                        return Err(EngineError::Unsupported(
                            "nth_value() n must be integer".into(),
                        ));
                    }
                };
                if raw < 1 {
                    return Err(EngineError::Unsupported(
                        "nth_value() n must be >= 1".into(),
                    ));
                }
                #[allow(clippy::cast_sign_loss)]
                {
                    raw as usize
                }
            } else {
                0
            };
            let eff = effective_frame(frame, ordered)?;
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice);
                let (_, _, idx) = &slice[i];
                let v = if lo > hi {
                    Value::Null
                } else {
                    match lower.as_str() {
                        "first_value" => values[lo].clone(),
                        "last_value" => values[hi].clone(),
                        "nth_value" => {
                            let pos = lo + nth - 1;
                            if pos > hi {
                                Value::Null
                            } else {
                                values[pos].clone()
                            }
                        }
                        _ => unreachable!(),
                    }
                };
                out_vals[*idx] = v;
            }
            Ok(())
        }
        "ntile" => {
            if args.is_empty() {
                return Err(EngineError::Unsupported(
                    "ntile(n) requires an integer argument".into(),
                ));
            }
            let v = eval::eval_expr(&args[0], filtered_rows[slice[0].2], ctx)
                .map_err(EngineError::Eval)?;
            let bucket_count: i64 = match v {
                Value::SmallInt(n) => i64::from(n),
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                _ => {
                    return Err(EngineError::Unsupported(
                        "ntile() argument must be integer".into(),
                    ));
                }
            };
            if bucket_count < 1 {
                return Err(EngineError::Unsupported(
                    "ntile() argument must be >= 1".into(),
                ));
            }
            #[allow(clippy::cast_sign_loss)]
            let buckets = bucket_count as usize;
            let n = slice.len();
            // Each bucket gets `base` rows; the first `extras` buckets
            // get one extra. PG semantics.
            let base = n / buckets;
            let extras = n % buckets;
            let mut bucket: usize = 1;
            let mut remaining_in_bucket = if extras > 0 { base + 1 } else { base };
            let mut buckets_with_extra_remaining = extras;
            for (_, _, idx) in slice {
                if remaining_in_bucket == 0 {
                    bucket += 1;
                    buckets_with_extra_remaining = buckets_with_extra_remaining.saturating_sub(1);
                    remaining_in_bucket = if buckets_with_extra_remaining > 0 {
                        base + 1
                    } else {
                        base
                    };
                    // Edge: if base==0 and extras==0, all rows fit;
                    // shouldn't reach here, but guard anyway.
                    if remaining_in_bucket == 0 {
                        remaining_in_bucket = 1;
                    }
                }
                out_vals[*idx] = Value::BigInt(i64::try_from(bucket).unwrap_or(i64::MAX));
                remaining_in_bucket -= 1;
            }
            Ok(())
        }
        "percent_rank" => {
            // (rank - 1) / (n - 1) where rank is the standard RANK().
            // Single-row partitions get 0.
            let n = slice.len();
            let mut prev_key: Option<&[(Value, bool)]> = None;
            let mut current_rank: i64 = 1;
            for (i, (_, okey, idx)) in slice.iter().enumerate() {
                if let Some(p) = prev_key
                    && order_key_cmp(p, okey) != core::cmp::Ordering::Equal
                {
                    current_rank = i64::try_from(i + 1).unwrap_or(i64::MAX);
                }
                if prev_key.is_none() {
                    current_rank = 1;
                }
                #[allow(clippy::cast_precision_loss)]
                let pr = if n <= 1 {
                    0.0
                } else {
                    (current_rank - 1) as f64 / (n - 1) as f64
                };
                out_vals[*idx] = Value::Float(pr);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "cume_dist" => {
            // # rows up to and including this row's peer group / n.
            let n = slice.len();
            // First pass: find peer-group-end rank for each row.
            for i in 0..slice.len() {
                let peer_end = peer_group_end(slice, i);
                #[allow(clippy::cast_precision_loss)]
                let cd = (peer_end + 1) as f64 / n as f64;
                let (_, _, idx) = &slice[i];
                out_vals[*idx] = Value::Float(cd);
            }
            Ok(())
        }
        other => Err(EngineError::Unsupported(alloc::format!(
            "window function {other:?} not supported (v4.21: row_number/rank/dense_rank/sum/avg/count/min/max/lag/lead/first_value/last_value/nth_value/ntile/percent_rank/cume_dist)"
        ))),
    }
}

/// v4.20: resolve the user-provided frame down to a normalised
/// `(kind, start, end)`. `None` means default — derive from
/// `ordered`: ordered ⇒ RANGE UNBOUNDED PRECEDING AND CURRENT ROW,
/// unordered ⇒ ROWS UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING.
/// Single-bound shorthand (e.g. `ROWS 5 PRECEDING`) normalises
/// end → CURRENT ROW per the PG spec.
fn effective_frame(
    frame: Option<&WindowFrame>,
    ordered: bool,
) -> Result<(FrameKind, FrameBound, FrameBound), EngineError> {
    match frame {
        None => {
            if ordered {
                Ok((
                    FrameKind::Range,
                    FrameBound::UnboundedPreceding,
                    FrameBound::CurrentRow,
                ))
            } else {
                Ok((
                    FrameKind::Rows,
                    FrameBound::UnboundedPreceding,
                    FrameBound::UnboundedFollowing,
                ))
            }
        }
        Some(fr) => {
            let end = fr.end.clone().unwrap_or(FrameBound::CurrentRow);
            // Reject start > end (a few impossible combinations).
            if matches!(fr.start, FrameBound::UnboundedFollowing)
                || matches!(end, FrameBound::UnboundedPreceding)
            {
                return Err(EngineError::Unsupported(alloc::format!(
                    "invalid frame: start={:?} end={:?}",
                    fr.start,
                    end
                )));
            }
            // RANGE OFFSET PRECEDING / FOLLOWING needs value-typed
            // arithmetic on the ORDER BY key (e.g. `RANGE BETWEEN
            // INTERVAL '1 day' PRECEDING AND CURRENT ROW`). Not
            // implemented in v4.20.
            if fr.kind == FrameKind::Range
                && (matches!(
                    fr.start,
                    FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_)
                ) || matches!(
                    end,
                    FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_)
                ))
            {
                return Err(EngineError::Unsupported(
                    "RANGE with explicit offset bounds is not supported (v4.20: only UNBOUNDED / CURRENT ROW for RANGE)".into(),
                ));
            }
            Ok((fr.kind, fr.start.clone(), end))
        }
    }
}

/// Compute `(lo, hi)` row-index bounds inside the partition slice
/// for the row at position `i`. Inclusive, clamped to
/// `[0, slice.len()-1]`. Empty result if `lo > hi`.
#[allow(clippy::type_complexity)]
fn frame_bounds_for_row(
    eff: &(FrameKind, FrameBound, FrameBound),
    i: usize,
    slice: &[(Vec<Value>, Vec<(Value, bool)>, usize)],
) -> (usize, usize) {
    let (kind, start, end) = eff;
    let n = slice.len();
    let last = n.saturating_sub(1);
    let (mut lo, mut hi) = match kind {
        FrameKind::Rows => {
            let lo = match start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::OffsetPreceding(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_sub(k)
                }
                FrameBound::CurrentRow => i,
                FrameBound::OffsetFollowing(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_add(k).min(last)
                }
                FrameBound::UnboundedFollowing => last,
            };
            let hi = match end {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::OffsetPreceding(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_sub(k)
                }
                FrameBound::CurrentRow => i,
                FrameBound::OffsetFollowing(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_add(k).min(last)
                }
                FrameBound::UnboundedFollowing => last,
            };
            (lo, hi)
        }
        FrameKind::Range => {
            // RANGE bounds are peer-aware. With only UNBOUNDED and
            // CURRENT ROW supported (rejected at effective_frame for
            // explicit offsets), the start/end map to the
            // partition's full extent at the same-order-key peer
            // group boundary.
            let lo = match start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::CurrentRow => peer_group_start(slice, i),
                FrameBound::UnboundedFollowing => last,
                _ => unreachable!("offset bounds rejected for RANGE"),
            };
            let hi = match end {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::CurrentRow => peer_group_end(slice, i),
                FrameBound::UnboundedFollowing => last,
                _ => unreachable!("offset bounds rejected for RANGE"),
            };
            (lo, hi)
        }
    };
    if hi >= n {
        hi = last;
    }
    if lo >= n {
        lo = last;
    }
    (lo, hi)
}

/// Find the inclusive index of the first row with the same ORDER
/// BY key as `slice[i]`. Slice is already sorted by partition then
/// order, so peers are contiguous.
#[allow(clippy::type_complexity)]
fn peer_group_start(slice: &[(Vec<Value>, Vec<(Value, bool)>, usize)], i: usize) -> usize {
    let key = &slice[i].1;
    let mut j = i;
    while j > 0 && order_key_cmp(&slice[j - 1].1, key) == core::cmp::Ordering::Equal {
        j -= 1;
    }
    j
}

/// Find the inclusive index of the last row with the same ORDER
/// BY key as `slice[i]`.
#[allow(clippy::type_complexity)]
fn peer_group_end(slice: &[(Vec<Value>, Vec<(Value, bool)>, usize)], i: usize) -> usize {
    let key = &slice[i].1;
    let mut j = i;
    while j + 1 < slice.len() && order_key_cmp(&slice[j + 1].1, key) == core::cmp::Ordering::Equal {
        j += 1;
    }
    j
}

fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::SmallInt(n) => Some(f64::from(*n)),
        Value::Int(n) => Some(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Some(*n as f64),
        Value::Float(x) => Some(*x),
        _ => None,
    }
}

/// Quick scan for any subquery-bearing node in a SELECT's WHERE /
/// projection / `order_by` — saves cloning the AST when there are
/// none (the common case).
fn expr_tree_has_subquery(stmt: &SelectStatement) -> bool {
    let mut any = false;
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            any = any || expr_has_subquery(expr);
        }
    }
    if let Some(w) = &stmt.where_ {
        any = any || expr_has_subquery(w);
    }
    if let Some(h) = &stmt.having {
        any = any || expr_has_subquery(h);
    }
    if let Some(o) = &stmt.order_by {
        any = any || expr_has_subquery(&o.expr);
    }
    for (_, peer) in &stmt.unions {
        any = any || expr_tree_has_subquery(peer);
    }
    any
}

fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        Expr::Binary { lhs, rhs, .. } => expr_has_subquery(lhs) || expr_has_subquery(rhs),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_has_subquery(expr)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_subquery),
        Expr::Like { expr, pattern, .. } => expr_has_subquery(expr) || expr_has_subquery(pattern),
        Expr::Extract { source, .. } => expr_has_subquery(source),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(expr_has_subquery)
                || partition_by.iter().any(expr_has_subquery)
                || order_by.iter().any(|(e, _)| expr_has_subquery(e))
        }
        Expr::Literal(_) | Expr::Column(_) => false,
    }
}

/// v4.10 helper: materialise a runtime `Value` back into an AST
/// `Expr::Literal` for the subquery-rewrite path. Supports the
/// types `Literal` can represent (Integer / Float / Text / Bool /
/// Null). Date / Timestamp / Numeric / Vector / Interval / JSON
/// would lose precision through Literal and aren't supported in
/// uncorrelated-subquery results; they error with a clear hint.
fn value_to_literal_expr(v: Value) -> Result<Expr, EngineError> {
    let lit = match v {
        Value::Null => Literal::Null,
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        Value::Text(s) | Value::Json(s) => Literal::String(s),
        Value::Bool(b) => Literal::Bool(b),
        other => {
            return Err(EngineError::Unsupported(alloc::format!(
                "subquery result type {:?} not yet materialisable; cast to text or integer in the inner SELECT",
                other.data_type()
            )));
        }
    };
    Ok(Expr::Literal(lit))
}

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
        // v4.10 subquery nodes — recurse into the inner SELECT's
        // expression slots so e.g. SELECT NOW() in a scalar
        // subquery picks up the same instant as the outer query.
        Expr::ScalarSubquery(s) => rewrite_select_clock(s, now),
        Expr::Exists { subquery, .. } => rewrite_select_clock(subquery, now),
        Expr::InSubquery { expr, subquery, .. } => {
            rewrite_expr_clock(expr, now);
            rewrite_select_clock(subquery, now);
        }
        // v4.12 window functions — args + PARTITION BY + ORDER BY
        // may all reference clock literals.
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                rewrite_expr_clock(a, now);
            }
            for p in partition_by {
                rewrite_expr_clock(p, now);
            }
            for (e, _) in order_by {
                rewrite_expr_clock(e, now);
            }
        }
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
        ColumnTypeName::Json => DataType::Json,
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
            // v4.9: Text ↔ JSON coercion. No structural validation —
            // any text literal is accepted; the responsibility for
            // valid JSON lies with the producer.
            (Value::Text(s), DataType::Json) => Some(Value::Json(s)),
            (Value::Json(s), DataType::Text) => Some(Value::Text(s)),
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
