//! AST for the PG-dialect subset SPG accepts in v0.2.
//!
//! `Display` is implemented so that for any AST `a` produced by [`crate::parser`],
//! re-parsing `format!("{a}")` yields a structurally equal AST. Binary and
//! unary operators always emit parentheses to remove any precedence
//! ambiguity — round-trip safety wins over prettiness.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // Statement::Select dominates; Boxing would touch every match site
pub enum Statement {
    Select(SelectStatement),
    CreateTable(CreateTableStatement),
    /// v7.9.15 — `CREATE EXTENSION [IF NOT EXISTS] <name>
    /// [WITH SCHEMA <s>] [VERSION <v>] [CASCADE]` accepted as a
    /// no-op so PG dumps that include extension declarations
    /// (notably `pgvector`) load against SPG without splitting
    /// init scripts. mailrs migration follow-up F3.
    CreateExtension(String),
    /// v7.9.27 — PG `DO $$ … $$ [LANGUAGE plpgsql];` block. SPG
    /// has no PL/pgSQL; engine returns CommandOk no-op so
    /// `pg_dump` output with idempotent DO migrations loads
    /// against SPG without splitting scripts. The lexer
    /// consumes the dollar-quoted body into a discarded
    /// Token::String. mailrs migration follow-up H1.
    DoBlock,
    CreateIndex(CreateIndexStatement),
    Insert(InsertStatement),
    /// v4.4 — `UPDATE <table> SET col=expr [, ...] [WHERE cond]`.
    Update(UpdateStatement),
    /// v4.4 — `DELETE FROM <table> [WHERE cond]`.
    Delete(DeleteStatement),
    Begin,
    Commit,
    Rollback,
    /// `SAVEPOINT <name>` — push a named savepoint onto the active TX's
    /// stack so a later `ROLLBACK TO <name>` can undo just the work
    /// since this point.
    Savepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] <name>` — restore catalog state to the
    /// named savepoint and discard later savepoints. Does not end the
    /// transaction.
    RollbackToSavepoint(String),
    /// `RELEASE [SAVEPOINT] <name>` — discard a savepoint without
    /// rolling back. Keeps the work done since then.
    ReleaseSavepoint(String),
    /// `SHOW TABLES` — return the list of tables in the catalog.
    ShowTables,
    /// `SHOW COLUMNS FROM <table>` — return one row per column with
    /// its declared name / type / nullability.
    ShowColumns(String),
    /// `CREATE USER 'name' WITH PASSWORD 'pw' ROLE 'admin'` (v4.1).
    /// Role is optional; defaults to `readonly` when omitted.
    CreateUser(CreateUserStatement),
    /// `DROP USER 'name'` (v4.1).
    DropUser(String),
    /// `SHOW USERS` (v4.1) — admin-only listing of (name, role).
    ShowUsers,
    /// v4.26 — `EXPLAIN [ANALYZE] <select>`. The engine returns a
    /// single-column text table describing the rewritten plan tree
    /// for `inner`. `analyze` triggers an actual exec to attach
    /// observed row counts and elapsed micros to each node.
    Explain(ExplainStatement),
    /// v6.0.4 — `ALTER INDEX <name> REBUILD [WITH (encoding = ...)]`.
    /// Synchronous rebuild of an NSW index. With the optional
    /// encoding clause, every stored cell at the indexed column is
    /// also re-encoded through `coerce_value` before the new graph
    /// builds.
    AlterIndex(AlterIndexStatement),
    /// v6.7.2 — `ALTER TABLE <name> SET <setting> = <value>`.
    /// The only setting in v6.7.2 is `hot_tier_bytes`, which
    /// overrides the global `SPG_HOT_TIER_BYTES` freezer trigger
    /// for the named table.
    AlterTable(AlterTableStatement),
    /// v6.1.2 — `CREATE PUBLICATION <name> [FOR ALL TABLES]`.
    /// The catalog row lives in `spg_publications`. Publisher-side
    /// WAL filtering arrives in v6.1.5.
    CreatePublication(CreatePublicationStatement),
    /// v6.1.2 — `DROP PUBLICATION <name>`. PG-compatible silent
    /// no-op when the publication does not exist.
    DropPublication(String),
    /// v6.1.3 — `SHOW PUBLICATIONS`. Returns one row per
    /// publication ordered by name with `(name, scope_summary,
    /// table_count)` columns. The scope summary is the human-
    /// readable form `ALL TABLES` / `FOR TABLE …` / `FOR ALL
    /// TABLES EXCEPT …`; `table_count` is `NULL` for the
    /// `AllTables` scope and the table-list length otherwise.
    ShowPublications,
    /// v6.1.4 — `CREATE SUBSCRIPTION <name> CONNECTION '<conn>'
    /// PUBLICATION <pub_name> [, <pub_name> …]`. Catalog lands
    /// in `spg_subscriptions`; when the subscription is
    /// `enabled = true` (default) the server spawns a
    /// background worker that connects to `conn` and drains the
    /// requested publication(s) into the local engine.
    CreateSubscription(CreateSubscriptionStatement),
    /// v6.1.4 — `DROP SUBSCRIPTION <name>`. Like DROP
    /// PUBLICATION, silent no-op when absent. Stops the
    /// associated worker thread before removing the row.
    DropSubscription(String),
    /// v6.1.4 — `SHOW SUBSCRIPTIONS`. Returns one row per
    /// subscription ordered by name with `(name, conn_str,
    /// publications, enabled, last_received_pos)`.
    ShowSubscriptions,
    /// v6.1.7 — `WAIT FOR WAL POSITION <pos> [WITH TIMEOUT <ms>]`.
    /// Blocks until the local server's apply position reaches
    /// `<pos>` or `<ms>` elapses. Server-layer command: the
    /// engine refuses it (`EngineError::Unsupported`) since
    /// `lag_state` lives in `spg-server`'s `ServerState`.
    WaitForWalPosition {
        pos: u64,
        /// `None` → wait forever; `Some(ms)` → return after `ms`
        /// milliseconds even if the target isn't reached.
        timeout_ms: Option<u64>,
    },
    /// v6.2.0 — `ANALYZE [<table>]`. Bare form walks every user
    /// table; `ANALYZE <name>` re-stats just one. Populates
    /// `spg_statistic` with per-column null_frac + n_distinct +
    /// 100-bucket equi-depth histogram.
    Analyze(Option<String>),
    /// v6.7.3 — `COMPACT COLD SEGMENTS`. Walks every user table's
    /// BTree-cold indices and merges small cold-tier segments
    /// (size below `SPG_COMPACTION_TARGET_SEGMENT_BYTES`, default
    /// 4 MiB) into a single larger segment per (table, index).
    /// `WHERE` predicate filtering on which tables to compact is
    /// carved out of v6.7.3 (per V6_7_DESIGN.md STABILITY entry);
    /// v6.7.3 only supports the bare form.
    CompactColdSegments,
    /// v7.12.1 — `SET <name> [TO|=] <value>`. Records a session
    /// parameter on the engine; v7.12.1 honours
    /// `default_text_search_config` (consumed by `to_tsvector` /
    /// `plainto_tsquery` family when called without an explicit
    /// config arg). All other names are accepted as a no-op so PG
    /// dumps with `SET client_encoding`, `SET search_path` etc.
    /// load cleanly.
    SetParameter {
        name: String,
        value: SetValue,
    },
    /// v7.12.1 — `RESET <name>` / `RESET ALL`. Restores parameter
    /// to its default. No-op for parameters SPG does not track.
    ResetParameter(Option<String>),
}

/// v7.12.1 — payload of a SET right-hand side. PG syntax accepts
/// a string literal, an identifier (often a config name), an
/// integer/float, or the bare `DEFAULT` keyword.
#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    String(String),
    Ident(String),
    Number(String),
    Default,
}

/// v6.1.4 — `CREATE SUBSCRIPTION` AST node. v6.1.4 ships a
/// single fixed-shape DDL; the WITH-clause options PG supports
/// (`enabled`, `slot_name`, `streaming`, `binary`) are out of
/// scope for v6.1.4 — `enabled` defaults to true and there are
/// no other knobs to set in v6.1.x.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSubscriptionStatement {
    pub name: String,
    /// Connection string in PG keyword=value form (e.g.
    /// `host=127.0.0.1 port=20002`). v6.1.4 only consumes the
    /// `host` and `port` fields; the rest is reserved for
    /// future v6.1.x options.
    pub conn_str: String,
    /// One or more publications on the remote side. Order is
    /// preserved verbatim from the DDL; the worker requests them
    /// in this order. v6.1.4 records the list; v6.1.5
    /// publisher-side filtering enforces it.
    pub publications: Vec<String>,
}

/// v6.1.2 — `CREATE PUBLICATION` AST node. The `scope` field uses
/// the [`PublicationScope`] shape. v6.1.2 only accepted
/// `AllTables`; v6.1.3 unlocks the `ForTables` / `AllTablesExcept`
/// variants by flipping the parser gate (no AST migration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePublicationStatement {
    pub name: String,
    pub scope: PublicationScope,
}

/// v6.1.2 — Which tables a publication covers. v6.1.3 (this commit)
/// flips the parser gate for the `ForTables` / `AllTablesExcept`
/// variants — the on-disk shape, snapshot serialisation, and the
/// AST round-trip Display path were already in place in v6.1.2
/// so this is a parser-only widening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationScope {
    AllTables,
    ForTables(Vec<String>),
    AllTablesExcept(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterIndexStatement {
    pub name: String,
    pub target: AlterIndexTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterIndexTarget {
    /// `REBUILD [WITH (encoding = <enc>)]`. `encoding = None`
    /// rebuilds the existing graph in place without touching the
    /// column encoding; `Some(enc)` re-encodes every cell first.
    Rebuild { encoding: Option<VecEncoding> },
}

/// v6.7.2 — `ALTER TABLE t SET <setting> = <value>`. v6.7.2 ships
/// the single `hot_tier_bytes` setting; later v6.7.x sub-versions
/// can add more SET subjects without changing the dispatch shape.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTableStatement {
    pub name: String,
    pub target: AlterTableTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableTarget {
    /// Per-table hot-tier byte budget override. The freezer
    /// reads this before falling back to `SPG_HOT_TIER_BYTES`.
    SetHotTierBytes(u64),
    /// v7.6.8 — `ALTER TABLE t ADD CONSTRAINT name FOREIGN KEY
    /// (cols) REFERENCES parent[(pcols)] [ON DELETE/UPDATE …]`.
    /// Engine validates existing rows against the new constraint
    /// before installing it.
    AddForeignKey(ForeignKeyConstraint),
    /// v7.6.8 — `ALTER TABLE t DROP CONSTRAINT name`. Removes the
    /// constraint by user-supplied name; raises if no FK with that
    /// name exists on the table.
    DropForeignKey(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainStatement {
    pub analyze: bool,
    pub inner: Box<SelectStatement>,
    /// v6.8.3 — `EXPLAIN (SUGGEST) <SELECT>` enables the index
    /// advisor pass: after the regular plan tree, the engine
    /// emits one suggestion line per column referenced in the
    /// query's WHERE / JOIN that has no covering index on the
    /// owning table.
    pub suggest: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateUserStatement {
    pub name: String,
    pub password: String,
    /// One of `admin` / `readwrite` / `readonly`. Stored verbatim from
    /// the parser; the engine validates against `Role::parse` so a
    /// typo lands as a runtime error with a clear message rather than
    /// a parse failure.
    pub role: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexStatement {
    pub name: String,
    pub table: String,
    pub column: String,
    /// Optional `USING <method>` clause. v2.0 recognises `hnsw` (NSW
    /// graph for vector kNN); unspecified is the default B-tree index.
    pub method: IndexMethod,
    /// `IF NOT EXISTS` — engine returns `CommandOk` no-op when the
    /// index name already exists, instead of raising `DuplicateIndex`.
    pub if_not_exists: bool,
    /// v6.8.0 — `INCLUDE (col1, col2, …)` columns. Identifies the
    /// non-key columns the planner should treat as "covered" by
    /// this index when checking whether a query can run as an
    /// index-only scan. Empty when no `INCLUDE` clause was given.
    pub included_columns: Vec<String>,
    /// v6.8.1 — `WHERE <expr>` partial-index predicate. Only rows
    /// for which `<expr>` evaluates truthy enter the index;
    /// queries whose `WHERE` clause's canonical Display form
    /// matches this expression's Display form can be served by the
    /// partial index. Stored as a parsed `Expr` so the engine
    /// re-uses the existing evaluation path; storage persists the
    /// Display form on the catalog snapshot.
    pub partial_predicate: Option<Expr>,
    /// v6.8.2 — expression-based index. When `Some(expr)`, the
    /// index key is the result of `expr` evaluated on each row
    /// (e.g. `CREATE INDEX … (lower(name))`). The `column`
    /// field still names the *primary* column the expression
    /// touches so existing planner shortcuts that resolve a
    /// column position stay valid. `None` = plain
    /// column-reference index (the legacy shape).
    pub expression: Option<Expr>,
    /// v7.9.14 — extra column names after the leading column in a
    /// multi-column `CREATE INDEX … (a, b, c)`. mailrs F2. The
    /// planner today still only uses the leading column for index
    /// seeks; the extras are tracked verbatim so the same DDL
    /// round-trips through WAL replay + catalog snapshot, and so
    /// the engine can emit a clear warning at INDEX CREATE time
    /// that only the leading column is currently honoured.
    /// Composite BTree index keys land in v7.10.
    pub extra_columns: Vec<String>,
    /// v7.9.29 — `CREATE UNIQUE INDEX …`. When true the engine
    /// enforces uniqueness on the indexed key (combined with the
    /// `partial_predicate` filter — only rows where the predicate
    /// evaluates truthy enter the uniqueness check). Standard SQL
    /// and PG's canonical way to express conditional uniqueness.
    /// mailrs K1.
    pub is_unique: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMethod {
    /// Default — B-tree over `IndexKey`. Used for equality / range
    /// lookups on scalar columns.
    BTree,
    /// `USING hnsw` — NSW graph for kNN over a vector column.
    Hnsw,
    /// v6.7.1 — `USING brin` — Block Range INdex. Per-segment
    /// metadata that records (min_key, max_key) for each page in a
    /// cold-tier segment, on the indexed column. The optimizer
    /// can use these summaries to skip pages whose range does NOT
    /// overlap a query's WHERE predicate. BRIN indexes carry no
    /// in-memory data — the summaries live in the segment v2
    /// envelope's sidecar. Created via the standard
    /// `CREATE INDEX … USING brin (col)` syntax.
    Brin,
    /// v7.12.3 — `USING gin` — inverted index over a `tsvector`
    /// column. Posting lists map `lexeme word` → row locators; the
    /// planner uses them to narrow `WHERE col @@ tsquery` to the
    /// candidate rows whose vectors contain a matching term, then
    /// re-evaluates the full `@@` semantics on each candidate.
    /// Replaces the v7.9.26b `USING gin` → BTree fallback that
    /// silently degraded to a full scan at query time.
    Gin,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStatement {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// `IF NOT EXISTS` — engine returns `CommandOk` no-op when the
    /// table name already exists, instead of raising `DuplicateTable`.
    pub if_not_exists: bool,
    /// v7.6.0 — table-level `FOREIGN KEY (...) REFERENCES ...`
    /// constraints. Column-level `REFERENCES` (single-column inline
    /// form) is normalised into this vec at parse time so the engine
    /// sees one uniform list.
    pub foreign_keys: Vec<ForeignKeyConstraint>,
    /// v7.9.18 — table-level constraints: `PRIMARY KEY (a, b)` and
    /// `UNIQUE (a, b, ...)`. mailrs migration follow-up G1 + G6.
    /// Engine resolves each into a BTree index named after the
    /// constraint's leading column at CREATE TABLE time; INSERT
    /// path enforces composite uniqueness via row scan on the
    /// leading column index.
    pub table_constraints: Vec<TableConstraint>,
}

/// v7.9.18 — table-level constraint at the end of a CREATE TABLE
/// column list. Either a composite PRIMARY KEY or a UNIQUE
/// (single- or multi-column).
#[derive(Debug, Clone, PartialEq)]
pub enum TableConstraint {
    /// `PRIMARY KEY (col1, col2, ...)`. Implies NOT NULL on each
    /// referenced column. Engine builds a BTree index named
    /// `<table>_pkey` and enforces composite uniqueness on INSERT.
    PrimaryKey {
        name: Option<String>,
        columns: Vec<String>,
    },
    /// `UNIQUE (col1, col2, ...)`. Engine builds a BTree index
    /// named `<table>_<leading_col>_key` (single-column) or
    /// `<table>_<leading_col>_<…>_key` (composite) and enforces
    /// uniqueness on INSERT.
    Unique {
        name: Option<String>,
        columns: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnTypeName,
    pub nullable: bool,
    /// `DEFAULT <expr>` literal supplied at CREATE TABLE. Engine
    /// evaluates this once (with an empty row) and caches the resulting
    /// `Value` on the column schema.
    pub default: Option<Expr>,
    /// MySQL-style `AUTO_INCREMENT` — the engine maintains a counter
    /// per such column and fills the slot when INSERT leaves it
    /// unbound (omitted from a column-list INSERT or explicitly NULL).
    pub auto_increment: bool,
    /// v7.9.13 — inline `PRIMARY KEY` column constraint. mailrs
    /// migration follow-up F1. Implies `NOT NULL`. Engine creates
    /// an implicit BTree index named `<table>_pkey` over this
    /// column at CREATE TABLE time, satisfying the parent-side
    /// index requirement for any FOREIGN KEY pointing at it.
    pub is_primary_key: bool,
}

/// v7.6.0 — A single FOREIGN KEY constraint. Both column-level
/// `REFERENCES` and table-level `FOREIGN KEY (...) REFERENCES ...`
/// parse into this shape — the column-level form has a single-entry
/// `columns` / `parent_columns`.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKeyConstraint {
    /// Optional `CONSTRAINT <name>` prefix. Engine ignores the name
    /// today but parses + stores it so a future ALTER TABLE DROP
    /// CONSTRAINT can target by name (v7.6.8).
    pub name: Option<String>,
    /// Local columns participating in the FK (≥ 1).
    pub columns: Vec<String>,
    /// Referenced parent table.
    pub parent_table: String,
    /// Referenced parent columns. Must have the same arity as
    /// `columns`; engine validates parent has a PK / UNIQUE index
    /// on exactly this column set (v7.6.1).
    pub parent_columns: Vec<String>,
    /// `ON DELETE` action. Defaults to `Restrict` if absent.
    pub on_delete: FkAction,
    /// `ON UPDATE` action. Defaults to `Restrict` if absent.
    pub on_update: FkAction,
}

/// v7.6.0 — Referential action for `ON DELETE` / `ON UPDATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FkAction {
    /// Reject the parent mutation if any child row references it.
    /// SQL spec default; SPG default when no clause is given.
    Restrict,
    /// Recursively propagate the parent's delete / update to the
    /// child rows. Same TX.
    Cascade,
    /// Set the child FK column(s) to NULL. Requires the FK columns
    /// to be NULL-able.
    SetNull,
    /// Set the child FK column(s) to their declared DEFAULT.
    /// Requires the child column(s) to have DEFAULT.
    SetDefault,
    /// SQL spec `NO ACTION` (deferred check). SPG treats this as
    /// `Restrict` because the single-writer model has no deferred
    /// constraint window; the keyword is accepted for compatibility.
    NoAction,
}

/// In-cell encoding for a `VECTOR(N)` column. v6.0.1 added the
/// optional `USING <encoding>` clause; omitting it keeps the
/// pre-v6 `F32` default. `Sq8` quantises each cell to a per-vector
/// affine `(min, max, [u8; dim])` triple (4× compression). `F16`
/// (v6.0.3, DDL keyword `HALF`) stores each element as IEEE-754
/// binary16 (2× compression, ~3 decimal digits of precision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VecEncoding {
    /// IEEE-754 binary32. Pre-v6 default; matches pgvector's
    /// uncompressed `vector` type wire / storage layout.
    #[default]
    F32,
    /// v6.0.1 SQ8 — per-vector affine 8-bit quantisation. See
    /// `spg_storage::quantize::Sq8Vector` for the math + recall
    /// envelope (≥ 0.95 on Gaussian / unit-sphere corpora at
    /// dim ≥ 32).
    Sq8,
    /// v6.0.3 halfvec — IEEE-754 binary16 (half-precision)
    /// per-element. DDL keyword `HALF` (pgvector convention).
    /// Bit-exact dequantise to f32 at the storage layer; no
    /// rerank pass needed for kNN search.
    F16,
}

impl fmt::Display for VecEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::F32 => f.write_str("F32"),
            Self::Sq8 => f.write_str("SQ8"),
            // pgvector convention: DDL keyword is `HALF`, not `F16`.
            Self::F16 => f.write_str("HALF"),
        }
    }
}

/// SQL-level type names. The mapping to the storage runtime's `DataType`
/// happens in `spg-engine` — keeping `spg-sql` free of storage deps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnTypeName {
    SmallInt,
    Int,
    BigInt,
    Float,
    Text,
    /// `VARCHAR(N)` — TEXT capped at N Unicode characters.
    Varchar(u32),
    /// `CHAR(N)` — TEXT right-padded with spaces to exactly N characters.
    Char(u32),
    Bool,
    /// pgvector fixed-dimension `VECTOR(N)`. v6.0.1 added the
    /// `USING <encoding>` clause; omitting it surfaces as
    /// `encoding = VecEncoding::F32` (the pre-v6 default).
    Vector {
        dim: u32,
        encoding: VecEncoding,
    },
    /// `NUMERIC` / `NUMERIC(p)` / `NUMERIC(p, s)` — exact decimal.
    /// Bare `NUMERIC` and `NUMERIC(p)` both surface with `scale=0`.
    Numeric(u8, u8),
    /// `DATE` — calendar day, no time-of-day component.
    Date,
    /// `TIMESTAMP` / `MySQL` `DATETIME` — instant with microsecond
    /// precision.
    Timestamp,
    /// v7.9.2 `TIMESTAMPTZ` / `TIMESTAMP WITH TIME ZONE`. SPG
    /// stores all timestamps as UTC microseconds-since-epoch and
    /// does not carry per-row offset (PG's internal representation
    /// is the same — TZ is a display convention). The distinction
    /// from `TIMESTAMP` exists for the PG-wire layer to advertise
    /// OID 1184 so sqlx-style clients decode into
    /// `chrono::DateTime<Utc>` instead of `NaiveDateTime`.
    Timestamptz,
    /// v4.9 `JSON` — text-backed JSON document. No parse-time
    /// validation; the engine round-trips the literal verbatim.
    /// PG OID 114 on the wire.
    Json,
    /// v7.9.0 `JSONB` — same storage shape as Json, advertised as
    /// PG OID 3802 on the wire so sqlx-style binary-typed clients
    /// decode without a custom type registration.
    Jsonb,
    /// v7.10.4 `BYTES` / `BYTEA` — raw binary blob. PG wire OID 17.
    /// Literal forms (decoded by the engine at coercion time):
    ///   - PG hex form: `'\xDEADBEEF'`
    ///   - Escape form: `'foo\\000bar'` (backslash octal triples)
    Bytes,
    /// v7.10.10 `TEXT[]` — single-dimension TEXT array. PG wire
    /// OID 1009. Literal forms accepted by the parser:
    ///   - `ARRAY['a', 'b', NULL]`
    ///   - `'{a,b,NULL}'::TEXT[]` (engine decodes the external
    ///     form at coerce time)
    TextArray,
    /// v7.11.13 `INT[]` — single-dimension i32 array. PG wire OID
    /// 1007. Same literal forms as TEXT[] (substituting integer
    /// elements).
    IntArray,
    /// v7.11.13 `BIGINT[]` — single-dimension i64 array. PG wire
    /// OID 1016.
    BigIntArray,
    /// v7.12.0 `tsvector` — PG full-text search lexeme set. PG
    /// wire OID 3614. Literal: `'foo:1 bar:2'::tsvector` (PG
    /// external form). G-CRIT-3.
    TsVector,
    /// v7.12.0 `tsquery` — PG full-text search parse tree. PG
    /// wire OID 3615.
    TsQuery,
}

impl fmt::Display for ColumnTypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SmallInt => f.write_str("SMALLINT"),
            Self::Int => f.write_str("INT"),
            Self::BigInt => f.write_str("BIGINT"),
            Self::Float => f.write_str("FLOAT"),
            Self::Text => f.write_str("TEXT"),
            Self::Varchar(n) => write!(f, "VARCHAR({n})"),
            Self::Char(n) => write!(f, "CHAR({n})"),
            Self::Bool => f.write_str("BOOL"),
            Self::Vector { dim, encoding } => match encoding {
                VecEncoding::F32 => write!(f, "VECTOR({dim})"),
                VecEncoding::Sq8 => write!(f, "VECTOR({dim}) USING SQ8"),
                VecEncoding::F16 => write!(f, "VECTOR({dim}) USING HALF"),
            },
            Self::Json => f.write_str("JSON"),
            Self::Jsonb => f.write_str("JSONB"),
            Self::Bytes => f.write_str("BYTEA"),
            Self::TextArray => f.write_str("TEXT[]"),
            Self::IntArray => f.write_str("INT[]"),
            Self::BigIntArray => f.write_str("BIGINT[]"),
            Self::TsVector => f.write_str("TSVECTOR"),
            Self::TsQuery => f.write_str("TSQUERY"),
            Self::Numeric(p, s) => {
                if *s == 0 {
                    write!(f, "NUMERIC({p})")
                } else {
                    write!(f, "NUMERIC({p}, {s})")
                }
            }
            Self::Date => f.write_str("DATE"),
            Self::Timestamp => f.write_str("TIMESTAMP"),
            Self::Timestamptz => f.write_str("TIMESTAMPTZ"),
        }
    }
}

/// `UPDATE <table> SET col = expr [, ...] [WHERE cond]`. v4.4 — the
/// engine evaluates `expr` per matched row in the table's row order
/// and rewrites cells in place. Indexed columns are dropped + re-
/// inserted into the affected B-tree on each row change.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatement {
    pub table: String,
    pub assignments: Vec<(String, Expr)>,
    pub where_: Option<Expr>,
    /// v7.9.4 — `RETURNING <projection>`. None = no RETURNING
    /// clause (legacy CommandComplete path). Some = engine
    /// evaluates the projection over each mutated row and
    /// streams the result as a Rows QueryResult.
    pub returning: Option<Vec<SelectItem>>,
}

/// `DELETE FROM <table> [WHERE cond]`. v4.4 — removes matched rows
/// from the active catalog and prunes them from every index.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub table: String,
    pub where_: Option<Expr>,
    /// v7.9.4 — `RETURNING <projection>`.
    pub returning: Option<Vec<SelectItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStatement {
    pub table: String,
    /// Optional column list — `INSERT INTO t (a, b) VALUES (...)`. When
    /// `None`, every tuple is positional and must match the table arity.
    /// When `Some`, the engine maps each tuple slot to the named column and
    /// fills the rest with NULL (must be nullable).
    pub columns: Option<Vec<String>>,
    /// One or more `(expr, expr, ...)` tuples — the multi-row VALUES form.
    /// v1.3+ accepts `INSERT INTO t VALUES (a), (b)`.
    pub rows: Vec<Vec<Expr>>,
    /// v7.9.7 — `ON CONFLICT (cols) DO { NOTHING | UPDATE SET … }`
    /// upsert clause. None = legacy INSERT (conflict raises a
    /// DuplicateKey error). mailrs migration blocker #2.
    pub on_conflict: Option<OnConflictClause>,
    /// v7.9.4 — `RETURNING <projection>`.
    pub returning: Option<Vec<SelectItem>>,
}

/// v7.9.7 — INSERT upsert clause: `ON CONFLICT (target) DO action`.
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflictClause {
    /// Local columns that identify the conflict (must match a
    /// UNIQUE / PRIMARY KEY index on the target table). Empty
    /// list means the user wrote `ON CONFLICT DO …` without a
    /// target — engine picks the table's first BTree index by
    /// convention.
    pub target_columns: Vec<String>,
    /// The action on conflict.
    pub action: OnConflictAction,
}

/// v7.9.7 — action on conflict.
#[derive(Debug, Clone, PartialEq)]
pub enum OnConflictAction {
    /// `DO NOTHING` — INSERT proceeds for non-conflicting rows,
    /// silently skips conflicting ones.
    Nothing,
    /// `DO UPDATE SET col = expr [, …] [WHERE cond]`. `assignments`
    /// may reference `EXCLUDED.col` to read the incoming row's
    /// value (engine wires `EXCLUDED` as a virtual table).
    Update {
        assignments: Vec<(String, Expr)>,
        where_: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    /// v4.11: `WITH name AS (SELECT ...) [, ...]` common-table
    /// expressions, materialised once at query start before the
    /// body SELECT runs. Empty for a regular SELECT. Non-recursive
    /// only — no `WITH RECURSIVE` for v4.x.
    pub ctes: Vec<Cte>,
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub where_: Option<Expr>,
    pub group_by: Option<Vec<Expr>>,
    /// v6.4.1 — `GROUP BY ALL` shortcut: when true, the planner
    /// expands `group_by` to every non-aggregate SELECT-list item
    /// before the executor runs. Mutually exclusive with an
    /// explicit `group_by` list (the parser sets exactly one).
    pub group_by_all: bool,
    /// `HAVING <expr>` — filter applied *after* `GROUP BY` aggregation.
    /// Supports aggregate calls (e.g. `HAVING count(*) > 1`); the
    /// aggregate executor resolves them through the same synthetic
    /// schema used for the SELECT items.
    pub having: Option<Expr>,
    /// UNION / UNION ALL chain. Empty for a plain SELECT. Each peer is
    /// itself a `SelectStatement` with `order_by = None` and `limit =
    /// None` (the parser enforces that — ORDER BY / LIMIT belong to the
    /// top of the chain).
    pub unions: Vec<(UnionKind, SelectStatement)>,
    /// v6.4.0 — multi-key ORDER BY. Empty `Vec` means no ORDER BY.
    /// Keys are matched left-to-right: first key decides, ties break
    /// to the second, etc.
    pub order_by: Vec<OrderBy>,
    /// `LIMIT <n>` — bound on row output. `n` is an integer
    /// literal **or** (v7.9.24) a placeholder `$N` resolved
    /// against the prepared-statement Bind values. mailrs
    /// migration follow-up H2.
    pub limit: Option<LimitExpr>,
    /// `OFFSET <n>` — drop the first `n` rows after ORDER BY but
    /// before LIMIT (so `LIMIT 10 OFFSET 5` keeps rows 6..=15).
    pub offset: Option<LimitExpr>,
}

/// v7.9.24 — LIMIT / OFFSET value. Integer literal at parse
/// time or a placeholder `$N` resolved during extended-query
/// Bind. mailrs migration follow-up H2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitExpr {
    /// `LIMIT 10` — value known at parse time.
    Literal(u32),
    /// `LIMIT $N` — the 1-based parameter index, resolved against
    /// the bind values when the prepared statement executes.
    Placeholder(u16),
}

impl fmt::Display for LimitExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(n) => write!(f, "{n}"),
            Self::Placeholder(n) => write!(f, "${n}"),
        }
    }
}

impl LimitExpr {
    /// Convenience for the simple-query path where no placeholders
    /// can possibly exist. Returns the literal value or `None` if
    /// this is a placeholder (caller must surface as Unsupported).
    pub fn as_literal(self) -> Option<u32> {
        match self {
            Self::Literal(n) => Some(n),
            Self::Placeholder(_) => None,
        }
    }
}

/// v7.9.24 — extract LIMIT / OFFSET as a `u32` literal. After
/// the engine's `substitute_placeholders` pass these are
/// always Literal; in the simple-query path a Placeholder
/// shape returns None (executor surfaces as
/// "LIMIT/OFFSET ${n} requires prepared-statement binding").
impl SelectStatement {
    #[must_use]
    pub fn limit_literal(&self) -> Option<u32> {
        self.limit.and_then(LimitExpr::as_literal)
    }
    #[must_use]
    pub fn offset_literal(&self) -> Option<u32> {
        self.offset.and_then(LimitExpr::as_literal)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    pub body: SelectStatement,
    /// v4.22: `WITH RECURSIVE` — set when the WITH clause had the
    /// RECURSIVE keyword. Applies to every CTE in the clause per
    /// PG semantics. A non-recursive body in a RECURSIVE WITH is
    /// allowed; the engine just runs it once.
    pub recursive: bool,
    /// v4.22: optional `WITH name(a, b, c)` column-name list. When
    /// non-empty, these override the body's output column names
    /// position-by-position; the engine errors out if the count
    /// doesn't match the body's projection width.
    pub column_overrides: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    /// `false` = ASC (default), `true` = DESC.
    pub desc: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnionKind {
    /// `UNION` — dedupes the combined set.
    Distinct,
    /// `UNION ALL` — concatenates without dedup.
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    /// v6.10.2 — `AS OF SEGMENT '<id>'` cold-tier time-travel.
    /// When `Some(id)`, the scan restricts to rows that live in
    /// segment `<id>` only — useful for forensic inspection of a
    /// specific freezer-emitted segment without exposing the hot
    /// tier. `AS OF TIMESTAMP <ts>` (PG-flavoured time travel)
    /// is STABILITY carve-out for v6.10 — needs the freezer to
    /// stamp each segment with a wall-clock at creation time.
    pub as_of_segment: Option<u32>,
    /// v7.11.7 — `FROM unnest(<expr>) [AS] <alias>` set-returning
    /// source. When `Some`, `name` is the alias (defaulting to
    /// `"unnest"` when no `AS` is given) and the engine builds a
    /// synthetic single-column table by evaluating the expression
    /// once at SELECT entry. Each TEXT[] element becomes one row;
    /// NULL elements become NULL cells. v7.11 supports
    /// uncorrelated UNNEST only (the expr cannot reference outer
    /// columns) and only as the FROM primary (no JOINs).
    pub unnest_expr: Option<Box<Expr>>,
}

/// FROM clause shape. v1.10 accepts a primary table plus a flat list of
/// joined peers — `FROM a [, b]* [INNER|LEFT] JOIN c ON expr ...`. The
/// joins evaluate left-associatively in nested-loop order.
#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    pub primary: TableRef,
    pub joins: Vec<FromJoin>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FromJoin {
    pub kind: JoinKind,
    pub table: TableRef,
    /// Required for INNER/LEFT; must be `None` for CROSS / comma-list.
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Column(ColumnName),
    /// v6.1.1 — `$N` parameter placeholder for the extended query
    /// protocol. The number is 1-based per PostgreSQL convention.
    /// Evaluation looks up `params[N-1]` from the prepared-statement
    /// bind buffer; out-of-range indices raise a runtime error
    /// (same shape as a column-not-found miss).
    Placeholder(u16),
    Binary {
        lhs: Box<Expr>,
        op: BinOp,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    /// PG-style `expr::TYPE` cast. v1.3 supports VECTOR, INT, BIGINT, FLOAT,
    /// TEXT, BOOL targets; engine coerces at evaluation time.
    Cast {
        expr: Box<Expr>,
        target: CastTarget,
    },
    /// Postfix `IS NULL` / `IS NOT NULL`. Returns BOOL.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// Function call `name(args...)`. v1.4 supports a small built-in set
    /// (length, upper, lower, abs, coalesce); unknown names error at eval
    /// time so the parser stays open for v1.5 aggregates.
    FunctionCall {
        name: String,
        args: Vec<Expr>,
    },
    /// SQL `LIKE` predicate. `pattern` evaluates to text at runtime;
    /// wildcards are `%` (any run) and `_` (one char), backslash escapes
    /// the next char (so `\%` matches a literal `%`).
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    /// v4.12 window function call: `name(args) OVER (PARTITION BY
    /// ... ORDER BY ...)`. Supports `ROW_NUMBER` / `RANK` /
    /// `DENSE_RANK` and the partition-aware aggregates `SUM` /
    /// `AVG` / `COUNT` / `MIN` / `MAX`. The window frame defaults to "entire partition" for
    /// unordered windows and "from start of partition through
    /// current row" for ordered windows — no explicit ROWS /
    /// RANGE clause in v4.12 MVP.
    WindowFunction {
        name: String,
        args: Vec<Expr>,
        partition_by: Vec<Expr>,
        order_by: Vec<(Expr, bool /* desc */)>,
        /// v4.20 explicit frame. `None` means "use the default":
        /// whole-partition when unordered, running aggregate from
        /// partition start through current row when ordered.
        frame: Option<WindowFrame>,
        /// v6.4.2 — `IGNORE NULLS` / `RESPECT NULLS` modifier on
        /// LAG / LEAD / FIRST_VALUE / LAST_VALUE. Default is
        /// `Respect` (PG / ANSI default — NULLs participate). Other
        /// window functions ignore this flag.
        null_treatment: NullTreatment,
    },
    /// v4.10 scalar subquery — `(SELECT ...)` used in expression
    /// position. Must return exactly one row × one column at eval
    /// time; the engine errors out otherwise. Uncorrelated only —
    /// the inner SELECT cannot reference outer columns.
    ScalarSubquery(Box<SelectStatement>),
    /// v4.10 `[NOT] EXISTS (SELECT ...)`. Returns Bool. Inner
    /// projection is ignored; only row-count matters.
    Exists {
        subquery: Box<SelectStatement>,
        negated: bool,
    },
    /// v4.10 `expr [NOT] IN (SELECT ...)`. Inner SELECT must
    /// project exactly one column; membership is tested by Eq
    /// against each row's value (NULL handling follows ANSI:
    /// NULL ∈ list ⇒ NULL ; otherwise present ⇒ true).
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<SelectStatement>,
        negated: bool,
    },
    /// `EXTRACT(<field> FROM <source>)` — pull an integer component
    /// out of a `DATE` or `TIMESTAMP`. Parsed as its own AST node
    /// because the `FROM` keyword is what separates the two halves,
    /// not a comma.
    Extract {
        field: ExtractField,
        source: Box<Expr>,
    },
    /// v7.10.10 — `ARRAY[expr, expr, …]` array constructor. Each
    /// element is evaluated independently; NULLs are allowed.
    /// v7.10 supports only single-dimension TEXT[] semantically;
    /// non-text elements coerce at engine evaluation time when
    /// the surrounding context (column type / cast) makes the
    /// target clear.
    Array(Vec<Expr>),
    /// v7.10.10 — array subscript `arr[i]`. PG 1-based; the
    /// engine returns NULL for out-of-range indices.
    ArraySubscript {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    /// v7.10.12 — `expr op ANY(arr)` and `expr op ALL(arr)`. The
    /// operator is the comparison binary op (Eq / Ne / Lt / …);
    /// the engine desugars: `ANY` returns true if any element
    /// satisfies; `ALL` returns true only if every element does.
    /// NULL handling follows PG's three-valued logic.
    AnyAll {
        expr: Box<Expr>,
        op: BinOp,
        array: Box<Expr>,
        /// `true` = ANY, `false` = ALL.
        is_any: bool,
    },
}

/// v6.4.2 — null treatment on `LAG` / `LEAD` / `FIRST_VALUE` /
/// `LAST_VALUE`. PG / ANSI default is `Respect` — NULLs participate
/// in the offset walk. `Ignore` causes the function to skip NULL
/// values in the argument expression, returning the next non-NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NullTreatment {
    #[default]
    Respect,
    Ignore,
}

/// v4.20 explicit window frame: `ROWS|RANGE BETWEEN <bound> AND
/// <bound>`. `end` is `None` for the shorthand "ROWS <bound>"
/// where end implicitly = CURRENT ROW.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowFrame {
    pub kind: FrameKind,
    pub start: FrameBound,
    pub end: Option<FrameBound>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Rows,
    Range,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBound {
    UnboundedPreceding,
    OffsetPreceding(u64),
    CurrentRow,
    OffsetFollowing(u64),
    UnboundedFollowing,
}

impl fmt::Display for FrameBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnboundedPreceding => f.write_str("UNBOUNDED PRECEDING"),
            Self::OffsetPreceding(n) => write!(f, "{n} PRECEDING"),
            Self::CurrentRow => f.write_str("CURRENT ROW"),
            Self::OffsetFollowing(n) => write!(f, "{n} FOLLOWING"),
            Self::UnboundedFollowing => f.write_str("UNBOUNDED FOLLOWING"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Microsecond,
}

impl fmt::Display for ExtractField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Year => "YEAR",
            Self::Month => "MONTH",
            Self::Day => "DAY",
            Self::Hour => "HOUR",
            Self::Minute => "MINUTE",
            Self::Second => "SECOND",
            Self::Microsecond => "MICROSECOND",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastTarget {
    Int,
    BigInt,
    Float,
    Text,
    Bool,
    Vector,
    Date,
    Timestamp,
    /// v7.9.25 — `::INTERVAL` and `::TIMESTAMPTZ`. mailrs follow-up
    /// H3a. Engine reuses the existing runtime-interval / timestamp
    /// paths (parse the text input, return the matching Value).
    Interval,
    Timestamptz,
    /// v7.9.25 — `::JSON` and `::JSONB`. SPG already has both
    /// types (v7.9.0); the cast just routes Text→Json with the
    /// requested OID for the wire layer.
    Json,
    Jsonb,
    /// v7.9.26 — `::regtype` / `::regclass`. Parsed for PG dump
    /// compatibility; engine surfaces as Unsupported with a
    /// hint to use `SHOW TABLES` or `spg_table_ddl`. mailrs F3b.
    RegType,
    RegClass,
    /// v7.10.11 — `::TEXT[]`. Engine decodes the LHS Text into
    /// the PG external array form `{a,b,NULL}`.
    TextArray,
    /// v7.11.13 — `::INT[]` / `::BIGINT[]`. Decodes PG external
    /// `{1,2,3}` or widens a `TextArray` whose elements are
    /// integer-shaped.
    IntArray,
    BigIntArray,
    /// v7.12.0 — `::tsvector` / `::tsquery`. Decodes the PG
    /// external form text representation. Used by pg_dump output
    /// and by `WHERE col @@ 'term'::tsquery` literal patterns.
    TsVector,
    TsQuery,
}

impl fmt::Display for CastTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int => "int",
            Self::BigInt => "bigint",
            Self::Float => "float",
            Self::Text => "text",
            Self::Bool => "bool",
            Self::Vector => "vector",
            Self::Interval => "interval",
            Self::Timestamptz => "timestamptz",
            Self::Json => "json",
            Self::Jsonb => "jsonb",
            Self::RegType => "regtype",
            Self::RegClass => "regclass",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::TextArray => "TEXT[]",
            Self::IntArray => "INT[]",
            Self::BigIntArray => "BIGINT[]",
            Self::TsVector => "tsvector",
            Self::TsQuery => "tsquery",
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Integer(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Null,
    /// pgvector-style array literal, e.g. `[1, 2.5, -3]`.
    Vector(Vec<f32>),
    /// `INTERVAL '<n> <unit> [<n> <unit> ...]'` — calendar-aware span.
    /// Split into a months part (because a month is not a fixed number of
    /// days) and a microseconds part (everything sub-month). `text` keeps
    /// the original spelling so Display round-trips byte-for-byte.
    Interval {
        months: i32,
        micros: i64,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnName {
    pub qualifier: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    NotEq,
    /// v7.9.27b — PG `a IS DISTINCT FROM b` / `a IS NOT DISTINCT
    /// FROM b`. NULL-safe equality: NULL IS NOT DISTINCT FROM
    /// NULL → true, NULL IS DISTINCT FROM NULL → false. The
    /// non-NULL behaviour matches `<>` / `=` exactly. Common in
    /// PG-style JOIN ON predicates and pg_dump output.
    IsDistinctFrom,
    IsNotDistinctFrom,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Add,
    Sub,
    Mul,
    Div,
    /// pgvector L2 (Euclidean) distance `<->`. Defined for two vector
    /// operands of equal dimension; engine returns `Value::Float(d)`.
    L2Distance,
    /// pgvector inner-product `<#>` — returns `-Σ aᵢ bᵢ` so "smaller =
    /// more similar" remains true (matches pgvector's published convention).
    InnerProduct,
    /// pgvector cosine distance `<=>` — `1 - (a·b)/(|a| |b|)`.
    CosineDistance,
    /// SQL string concatenation `||`. NULL propagates.
    Concat,
    /// v4.14 `json -> key` — element access by string key (object)
    /// or integer index (array). Returns a JSON value.
    JsonGet,
    /// v4.14 `json ->> key` — same access, returns the result as
    /// TEXT (unwraps a top-level JSON string; renders other scalars
    /// as their canonical text).
    JsonGetText,
    /// v6.4.5 `json #> path_text` — walk the path encoded as a PG
    /// text array literal like `'{a,0,b}'`. Returns JSON.
    JsonGetPath,
    /// v6.4.5 `json #>> path_text` — same walk, returns TEXT.
    JsonGetPathText,
    /// v6.4.5 `json @> sub_json` — containment. Returns BOOL; true
    /// when every key/value in `sub_json` is structurally present in
    /// the left side. Matches PG semantics (top-level + recursive).
    JsonContains,
    /// v7.12.2 `tsvector @@ tsquery` — FTS match. Returns BOOL;
    /// 3VL on NULL. Symmetric: PG also accepts `tsquery @@
    /// tsvector` and engine eval normalises either ordering.
    TsMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

// --- Display impls (round-trip-safe) --------------------------------------

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Select(s) => s.fmt(f),
            Self::CreateTable(s) => s.fmt(f),
            Self::CreateIndex(s) => s.fmt(f),
            Self::Insert(s) => s.fmt(f),
            Self::Update(s) => s.fmt(f),
            Self::Delete(s) => s.fmt(f),
            Self::Begin => f.write_str("BEGIN"),
            Self::Commit => f.write_str("COMMIT"),
            Self::Rollback => f.write_str("ROLLBACK"),
            Self::Savepoint(n) => write!(f, "SAVEPOINT {}", quote_ident(n)),
            Self::RollbackToSavepoint(n) => write!(f, "ROLLBACK TO SAVEPOINT {}", quote_ident(n)),
            Self::ReleaseSavepoint(n) => write!(f, "RELEASE SAVEPOINT {}", quote_ident(n)),
            Self::ShowTables => f.write_str("SHOW TABLES"),
            Self::ShowColumns(t) => write!(f, "SHOW COLUMNS FROM {}", quote_ident(t)),
            Self::CreateUser(s) => write!(
                f,
                "CREATE USER {} WITH PASSWORD '<redacted>' ROLE '{}'",
                quote_ident(&s.name),
                s.role
            ),
            Self::DropUser(n) => write!(f, "DROP USER {}", quote_ident(n)),
            Self::ShowUsers => f.write_str("SHOW USERS"),
            Self::ShowPublications => f.write_str("SHOW PUBLICATIONS"),
            Self::ShowSubscriptions => f.write_str("SHOW SUBSCRIPTIONS"),
            Self::CreateSubscription(s) => {
                write!(
                    f,
                    "CREATE SUBSCRIPTION {} CONNECTION '{}' PUBLICATION ",
                    quote_ident(&s.name),
                    s.conn_str.replace('\'', "''")
                )?;
                for (i, p) in s.publications.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", quote_ident(p))?;
                }
                Ok(())
            }
            Self::DropSubscription(name) => {
                write!(f, "DROP SUBSCRIPTION {}", quote_ident(name))
            }
            Self::WaitForWalPosition { pos, timeout_ms } => {
                write!(f, "WAIT FOR WAL POSITION {pos}")?;
                if let Some(ms) = timeout_ms {
                    write!(f, " WITH TIMEOUT {ms}")?;
                }
                Ok(())
            }
            Self::Analyze(None) => f.write_str("ANALYZE"),
            Self::Analyze(Some(t)) => write!(f, "ANALYZE {}", quote_ident(t)),
            Self::CompactColdSegments => f.write_str("COMPACT COLD SEGMENTS"),
            Self::Explain(e) => {
                if e.suggest {
                    write!(f, "EXPLAIN (SUGGEST) {}", e.inner)
                } else if e.analyze {
                    write!(f, "EXPLAIN ANALYZE {}", e.inner)
                } else {
                    write!(f, "EXPLAIN {}", e.inner)
                }
            }
            Self::AlterIndex(a) => {
                write!(f, "ALTER INDEX {} ", quote_ident(&a.name))?;
                match a.target {
                    AlterIndexTarget::Rebuild { encoding } => {
                        f.write_str("REBUILD")?;
                        if let Some(enc) = encoding {
                            write!(f, " WITH (encoding = {enc})")?;
                        }
                        Ok(())
                    }
                }
            }
            Self::AlterTable(a) => {
                write!(f, "ALTER TABLE {} ", quote_ident(&a.name))?;
                match &a.target {
                    AlterTableTarget::SetHotTierBytes(n) => {
                        write!(f, "SET hot_tier_bytes = {n}")
                    }
                    AlterTableTarget::AddForeignKey(fk) => write!(f, "ADD {fk}"),
                    AlterTableTarget::DropForeignKey(name) => {
                        write!(f, "DROP CONSTRAINT {}", quote_ident(name))
                    }
                }
            }
            Self::CreatePublication(p) => {
                write!(f, "CREATE PUBLICATION {}", quote_ident(&p.name))?;
                match &p.scope {
                    PublicationScope::AllTables => f.write_str(" FOR ALL TABLES"),
                    PublicationScope::ForTables(ts) => {
                        f.write_str(" FOR TABLE ")?;
                        for (i, t) in ts.iter().enumerate() {
                            if i > 0 {
                                f.write_str(", ")?;
                            }
                            write!(f, "{}", quote_ident(t))?;
                        }
                        Ok(())
                    }
                    PublicationScope::AllTablesExcept(ts) => {
                        f.write_str(" FOR ALL TABLES EXCEPT ")?;
                        for (i, t) in ts.iter().enumerate() {
                            if i > 0 {
                                f.write_str(", ")?;
                            }
                            write!(f, "{}", quote_ident(t))?;
                        }
                        Ok(())
                    }
                }
            }
            Self::CreateExtension(name) => {
                write!(f, "CREATE EXTENSION IF NOT EXISTS {}", quote_ident(name))
            }
            Self::DoBlock => f.write_str("DO $$ /* SPG no-op */ $$"),
            Self::DropPublication(name) => {
                write!(f, "DROP PUBLICATION {}", quote_ident(name))
            }
            Self::SetParameter { name, value } => {
                write!(f, "SET {name} = ")?;
                match value {
                    SetValue::String(s) => write!(f, "'{}'", s.replace('\'', "''")),
                    SetValue::Ident(s) | SetValue::Number(s) => f.write_str(s),
                    SetValue::Default => f.write_str("DEFAULT"),
                }
            }
            Self::ResetParameter(None) => f.write_str("RESET ALL"),
            Self::ResetParameter(Some(name)) => write!(f, "RESET {name}"),
        }
    }
}

impl fmt::Display for CreateIndexStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_unique {
            f.write_str("CREATE UNIQUE INDEX ")?;
        } else {
            f.write_str("CREATE INDEX ")?;
        }
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(
            f,
            "{} ON {} ",
            quote_ident(&self.name),
            quote_ident(&self.table)
        )?;
        match self.method {
            IndexMethod::Hnsw => f.write_str("USING hnsw ")?,
            IndexMethod::Brin => f.write_str("USING brin ")?,
            IndexMethod::Gin => f.write_str("USING gin ")?,
            IndexMethod::BTree => {}
        }
        if let Some(expr) = &self.expression {
            write!(f, "({})", expr)?;
        } else if self.extra_columns.is_empty() {
            write!(f, "({})", quote_ident(&self.column))?;
        } else {
            // v7.9.14 — multi-column key. Emit each column quoted
            // so the round-tripped form re-parses to identical AST.
            f.write_str("(")?;
            write!(f, "{}", quote_ident(&self.column))?;
            for c in &self.extra_columns {
                write!(f, ", {}", quote_ident(c))?;
            }
            f.write_str(")")?;
        }
        if !self.included_columns.is_empty() {
            f.write_str(" INCLUDE (")?;
            for (i, c) in self.included_columns.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", quote_ident(c))?;
            }
            f.write_str(")")?;
        }
        if let Some(pred) = &self.partial_predicate {
            write!(f, " WHERE {}", pred)?;
        }
        Ok(())
    }
}

impl fmt::Display for CreateTableStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE TABLE ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(f, "{} (", quote_ident(&self.name))?;
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{col}")?;
        }
        // v7.6.0 — render FK constraints in table-level form, after
        // the column list. WAL replay round-trips through Display, so
        // every FK must serialise here for replay to reconstruct the
        // schema bit-for-bit.
        for fk in &self.foreign_keys {
            f.write_str(", ")?;
            write!(f, "{fk}")?;
        }
        f.write_str(")")
    }
}

impl fmt::Display for ForeignKeyConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.name {
            write!(f, "CONSTRAINT {} ", quote_ident(name))?;
        }
        f.write_str("FOREIGN KEY (")?;
        for (i, c) in self.columns.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(&quote_ident(c))?;
        }
        write!(f, ") REFERENCES {}", quote_ident(&self.parent_table))?;
        if !self.parent_columns.is_empty() {
            f.write_str(" (")?;
            for (i, c) in self.parent_columns.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(&quote_ident(c))?;
            }
            f.write_str(")")?;
        }
        // Only render non-default actions to keep Display output
        // close to user input. SPG's default is RESTRICT (matches
        // SQL spec).
        if self.on_delete != FkAction::Restrict {
            write!(f, " ON DELETE {}", self.on_delete)?;
        }
        if self.on_update != FkAction::Restrict {
            write!(f, " ON UPDATE {}", self.on_update)?;
        }
        Ok(())
    }
}

impl fmt::Display for FkAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Restrict => f.write_str("RESTRICT"),
            Self::Cascade => f.write_str("CASCADE"),
            Self::SetNull => f.write_str("SET NULL"),
            Self::SetDefault => f.write_str("SET DEFAULT"),
            Self::NoAction => f.write_str("NO ACTION"),
        }
    }
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", quote_ident(&self.name), self.ty)?;
        if let Some(d) = &self.default {
            write!(f, " DEFAULT {d}")?;
        }
        if self.auto_increment {
            f.write_str(" AUTO_INCREMENT")?;
        }
        if !self.nullable {
            f.write_str(" NOT NULL")?;
        }
        Ok(())
    }
}

impl fmt::Display for InsertStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "INSERT INTO {}", quote_ident(&self.table))?;
        if let Some(cols) = &self.columns {
            f.write_str(" (")?;
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(&quote_ident(c))?;
            }
            f.write_str(")")?;
        }
        f.write_str(" VALUES ")?;
        for (ri, row) in self.rows.iter().enumerate() {
            if ri > 0 {
                f.write_str(", ")?;
            }
            f.write_str("(")?;
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{v}")?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl fmt::Display for UpdateStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UPDATE {} SET ", quote_ident(&self.table))?;
        for (i, (col, expr)) in self.assignments.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{} = {expr}", quote_ident(col))?;
        }
        if let Some(w) = &self.where_ {
            write!(f, " WHERE {w}")?;
        }
        Ok(())
    }
}

impl fmt::Display for DeleteStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DELETE FROM {}", quote_ident(&self.table))?;
        if let Some(w) = &self.where_ {
            write!(f, " WHERE {w}")?;
        }
        Ok(())
    }
}

impl fmt::Display for SelectStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_bare_select(self, f)?;
        for (kind, peer) in &self.unions {
            f.write_str(match kind {
                UnionKind::Distinct => " UNION ",
                UnionKind::All => " UNION ALL ",
            })?;
            write_bare_select(peer, f)?;
        }
        if !self.order_by.is_empty() {
            f.write_str(" ORDER BY ")?;
            for (i, o) in self.order_by.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", o.expr)?;
                if o.desc {
                    f.write_str(" DESC")?;
                }
            }
        }
        if let Some(n) = &self.limit {
            write!(f, " LIMIT {n}")?;
        }
        if let Some(o) = &self.offset {
            write!(f, " OFFSET {o}")?;
        }
        Ok(())
    }
}

fn write_bare_select(s: &SelectStatement, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("SELECT ")?;
    if s.distinct {
        f.write_str("DISTINCT ")?;
    }
    write_bare_select_body(s, f)
}

fn write_bare_select_body(s: &SelectStatement, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for (i, item) in s.items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    if let Some(t) = &s.from {
        write!(f, " FROM {t}")?;
    }
    if let Some(e) = &s.where_ {
        write!(f, " WHERE {e}")?;
    }
    if let Some(gs) = &s.group_by {
        f.write_str(" GROUP BY ")?;
        for (i, g) in gs.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{g}")?;
        }
    }
    if let Some(h) = &s.having {
        write!(f, " HAVING {h}")?;
    }
    Ok(())
}

impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wildcard => f.write_str("*"),
            Self::Expr { expr, alias } => {
                write!(f, "{expr}")?;
                if let Some(a) = alias {
                    write!(f, " AS {}", quote_ident(a))?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for FromClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.primary)?;
        for j in &self.joins {
            match j.kind {
                JoinKind::Inner => write!(f, " INNER JOIN {}", j.table)?,
                JoinKind::Left => write!(f, " LEFT JOIN {}", j.table)?,
                JoinKind::Cross => write!(f, " CROSS JOIN {}", j.table)?,
            }
            if let Some(on) = &j.on {
                write!(f, " ON {on}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", quote_ident(&self.name))?;
        if let Some(a) = &self.alias {
            write!(f, " AS {}", quote_ident(a))?;
        }
        Ok(())
    }
}

impl fmt::Display for ColumnName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(q) = &self.qualifier {
            write!(f, "{}.{}", quote_ident(q), quote_ident(&self.name))
        } else {
            write!(f, "{}", quote_ident(&self.name))
        }
    }
}

impl fmt::Display for Expr {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(l) => write!(f, "{l}"),
            Self::Column(c) => write!(f, "{c}"),
            Self::Placeholder(n) => write!(f, "${n}"),
            Self::Binary { lhs, op, rhs } => write!(f, "({lhs} {op} {rhs})"),
            Self::Unary { op, expr } => match op {
                UnOp::Not => write!(f, "(NOT {expr})"),
                UnOp::Neg => write!(f, "(-{expr})"),
            },
            Self::Cast { expr, target } => write!(f, "({expr}::{target})"),
            Self::IsNull { expr, negated } => {
                if *negated {
                    write!(f, "({expr} IS NOT NULL)")
                } else {
                    write!(f, "({expr} IS NULL)")
                }
            }
            Self::FunctionCall { name, args } => {
                write!(f, "{name}(")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(")")
            }
            Self::Like {
                expr,
                pattern,
                negated,
            } => {
                if *negated {
                    write!(f, "({expr} NOT LIKE {pattern})")
                } else {
                    write!(f, "({expr} LIKE {pattern})")
                }
            }
            Self::Extract { field, source } => write!(f, "EXTRACT({field} FROM {source})"),
            Self::WindowFunction {
                name,
                args,
                partition_by,
                order_by,
                frame,
                null_treatment: _,
            } => {
                write!(f, "{name}(")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(") OVER (")?;
                if !partition_by.is_empty() {
                    f.write_str("PARTITION BY ")?;
                    for (i, p) in partition_by.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{p}")?;
                    }
                }
                if !order_by.is_empty() {
                    if !partition_by.is_empty() {
                        f.write_str(" ")?;
                    }
                    f.write_str("ORDER BY ")?;
                    for (i, (e, desc)) in order_by.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{e}")?;
                        if *desc {
                            f.write_str(" DESC")?;
                        }
                    }
                }
                if let Some(fr) = frame {
                    if !partition_by.is_empty() || !order_by.is_empty() {
                        f.write_str(" ")?;
                    }
                    let k = match fr.kind {
                        FrameKind::Rows => "ROWS",
                        FrameKind::Range => "RANGE",
                    };
                    if let Some(end) = &fr.end {
                        write!(f, "{k} BETWEEN {} AND {}", fr.start, end)?;
                    } else {
                        write!(f, "{k} {}", fr.start)?;
                    }
                }
                f.write_str(")")
            }
            Self::ScalarSubquery(s) => write!(f, "({s})"),
            Self::Exists { subquery, negated } => {
                if *negated {
                    write!(f, "NOT EXISTS ({subquery})")
                } else {
                    write!(f, "EXISTS ({subquery})")
                }
            }
            Self::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                if *negated {
                    write!(f, "({expr} NOT IN ({subquery}))")
                } else {
                    write!(f, "({expr} IN ({subquery}))")
                }
            }
            Self::Array(items) => {
                f.write_str("ARRAY[")?;
                for (i, e) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{e}")?;
                }
                f.write_str("]")
            }
            Self::ArraySubscript { target, index } => write!(f, "({target}[{index}])"),
            Self::AnyAll {
                expr,
                op,
                array,
                is_any,
            } => {
                let kw = if *is_any { "ANY" } else { "ALL" };
                write!(f, "({expr} {op} {kw}({array}))")
            }
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(n) => write!(f, "{n}"),
            Self::Float(x) => {
                let s = format!("{x}");
                // Default Display for an integral f64 (e.g. 1.0) emits "1",
                // which would round-trip back to Integer. Force a dot.
                if s.contains('.') || s.contains('e') || s.contains('E') {
                    f.write_str(&s)
                } else {
                    write!(f, "{s}.0")
                }
            }
            Self::String(s) => {
                f.write_str("'")?;
                for c in s.chars() {
                    if c == '\'' {
                        f.write_str("''")?;
                    } else {
                        write!(f, "{c}")?;
                    }
                }
                f.write_str("'")
            }
            Self::Bool(b) => f.write_str(if *b { "TRUE" } else { "FALSE" }),
            Self::Null => f.write_str("NULL"),
            Self::Vector(v) => {
                f.write_str("[")?;
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    let s = format!("{x}");
                    // Mirror Float Display: force a dot so re-parse stays
                    // numerically literal.
                    if s.contains('.') || s.contains('e') || s.contains('E') {
                        f.write_str(&s)?;
                    } else {
                        write!(f, "{s}.0")?;
                    }
                }
                f.write_str("]")
            }
            Self::Interval { text, .. } => {
                f.write_str("INTERVAL '")?;
                for c in text.chars() {
                    if c == '\'' {
                        f.write_str("''")?;
                    } else {
                        write!(f, "{c}")?;
                    }
                }
                f.write_str("'")
            }
        }
    }
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Or => "OR",
            Self::And => "AND",
            Self::Eq => "=",
            Self::NotEq => "<>",
            Self::IsDistinctFrom => "IS DISTINCT FROM",
            Self::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::L2Distance => "<->",
            Self::InnerProduct => "<#>",
            Self::CosineDistance => "<=>",
            Self::Concat => "||",
            Self::JsonGet => "->",
            Self::JsonGetText => "->>",
            Self::JsonGetPath => "#>",
            Self::JsonGetPathText => "#>>",
            Self::JsonContains => "@>",
            Self::TsMatch => "@@",
        })
    }
}

/// Quote `s` as a PG double-quoted identifier when required (keyword,
/// non-folded case, leading digit, embedded non-`[A-Za-z0-9_]`, empty).
/// Otherwise return it as-is. Returns an owned `String` to keep the call site
/// uniform.
fn quote_ident(s: &str) -> String {
    let needs_quote = match s.chars().next() {
        None => true,
        Some(c) if !c.is_ascii_alphabetic() && c != '_' => true,
        _ => {
            s.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                || s.chars().any(|c| c.is_ascii_uppercase())
                || is_keyword(s)
        }
    };
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push_str("\"\"");
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

fn is_keyword(s: &str) -> bool {
    matches!(
        &*s.to_ascii_lowercase(),
        "select"
            | "from"
            | "where"
            | "as"
            | "null"
            | "true"
            | "false"
            | "and"
            | "or"
            | "not"
            | "create"
            | "table"
            | "insert"
            | "into"
            | "values"
            | "index"
            | "on"
            | "begin"
            | "commit"
            | "rollback"
            | "is"
            | "between"
            | "in"
            | "like"
            | "group"
            | "distinct"
            | "union"
            | "all"
            | "join"
            | "inner"
            | "left"
            | "cross"
            | "outer"
            | "default"
            | "savepoint"
            | "release"
            | "to"
            | "having"
            | "show"
            | "extract"
            | "offset"
            | "asc"
            | "desc"
            | "interval"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn integer_literal_renders_without_dot() {
        assert_eq!(Literal::Integer(42).to_string(), "42");
    }

    #[test]
    fn integral_float_keeps_dot() {
        assert_eq!(Literal::Float(1.0).to_string(), "1.0");
        assert_eq!(Literal::Float(1.5).to_string(), "1.5");
        assert_eq!(Literal::Float(2.5e-3).to_string(), "0.0025");
    }

    #[test]
    fn string_literal_doubles_quote() {
        assert_eq!(Literal::String("it's".into()).to_string(), "'it''s'");
    }

    #[test]
    fn bool_and_null_render_uppercase() {
        assert_eq!(Literal::Bool(true).to_string(), "TRUE");
        assert_eq!(Literal::Bool(false).to_string(), "FALSE");
        assert_eq!(Literal::Null.to_string(), "NULL");
    }

    #[test]
    fn binary_op_always_parenthesised() {
        let e = Expr::Binary {
            lhs: Box::new(Expr::Literal(Literal::Integer(1))),
            op: BinOp::Add,
            rhs: Box::new(Expr::Literal(Literal::Integer(2))),
        };
        assert_eq!(e.to_string(), "(1 + 2)");
    }

    #[test]
    fn select_star_from_table() {
        let s = SelectStatement {
            items: vec![SelectItem::Wildcard],
            from: Some(FromClause {
                primary: TableRef {
                    name: "users".into(),
                    alias: None,
                    as_of_segment: None,
                    unnest_expr: None,
                },
                joins: vec![],
            }),
            where_: None,
            group_by: None,
            group_by_all: false,
            having: None,
            unions: vec![],
            order_by: Vec::new(),
            limit: None,
            offset: None,
            distinct: false,
            ctes: vec![],
        };
        assert_eq!(s.to_string(), "SELECT * FROM users");
    }

    #[test]
    fn quote_ident_for_uppercase_and_keyword() {
        assert_eq!(quote_ident("foo"), "foo");
        assert_eq!(quote_ident("Foo"), "\"Foo\"");
        assert_eq!(quote_ident("select"), "\"select\"");
        assert_eq!(quote_ident(""), "\"\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
