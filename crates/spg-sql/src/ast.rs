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
    /// v7.14.0 — `DROP TABLE [IF EXISTS] name [, name…]
    /// [CASCADE | RESTRICT]`. Engine removes the matching tables
    /// (each one) from the catalog; IF EXISTS makes the drop
    /// idempotent. CASCADE / RESTRICT trailers parsed silently
    /// (SPG always cascades index drops on table drop).
    DropTable {
        names: Vec<String>,
        if_exists: bool,
    },
    /// v7.14.0 — `DROP INDEX [IF EXISTS] name`. Removes the
    /// matching index across whichever table holds it.
    DropIndex {
        name: String,
        if_exists: bool,
    },
    /// v7.14.0 — empty / comment-only statement. The lexer strips
    /// `--` line comments and `/* … */` block comments (including
    /// the MySQL conditional `/*!NNNNN … */` form) before the
    /// parser ever sees them; a SQL chunk that contains nothing
    /// else lands here. Engine returns CommandOk no-op so
    /// pg_dump / mysqldump preambles (`SET NAMES utf8mb4`
    /// wrapped in conditional comments, etc.) load cleanly.
    Empty,
    Select(SelectStatement),
    CreateTable(CreateTableStatement),
    /// v7.9.15 — `CREATE EXTENSION [IF NOT EXISTS] <name>
    /// [WITH SCHEMA <s>] [VERSION <v>] [CASCADE]` accepted as a
    /// no-op so PG dumps that include extension declarations
    /// (notably `pgvector`) load against SPG without splitting
    /// init scripts. mailrs migration follow-up F3.
    CreateExtension(String),
    /// v7.9.27 → v7.16.2 — PG `DO $$ … $$ [LANGUAGE plpgsql];`
    /// block. The body is now CAPTURED as a [`PlPgSqlBlock`] and
    /// the engine executes it at top level (mailrs round-10
    /// A.2). Pre-v7.16.2 the parser discarded the body and the
    /// engine returned CommandOk — a SEV-1 silent no-op that
    /// turned mailrs's `DO BEGIN IF EXISTS … THEN ALTER … END
    /// $$` idempotent migrations into invisible no-ops.
    DoBlock(PlPgSqlBlock),
    CreateIndex(CreateIndexStatement),
    Insert(InsertStatement),
    /// v4.4 — `UPDATE <table> SET col=expr [, ...] [WHERE cond]`.
    Update(UpdateStatement),
    /// v4.4 — `DELETE FROM <table> [WHERE cond]`.
    Delete(DeleteStatement),
    /// v7.17.0 Phase 3.P0-42 — SQL:2003 / PG 15+ `MERGE` statement.
    /// `MERGE INTO target [alias] USING source [alias] ON cond
    /// WHEN MATCHED [AND cond] THEN { UPDATE SET … | DELETE | DO NOTHING }
    /// WHEN NOT MATCHED [AND cond] THEN { INSERT (cols) VALUES (vals) | DO NOTHING }
    /// [WHEN …]`. SPG v7.17 supports table-based source (subquery
    /// source is a follow-up); BY SOURCE / BY TARGET and RETURNING
    /// are also follow-ups.
    Merge(MergeStatement),
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
    /// v7.14.0 — `SET a = 1, b = 2, …` MySQL-flavoured
    /// multi-assignment (mysqldump preamble uses
    /// `SET @OLD_FOREIGN_KEY_CHECKS = @@FOREIGN_KEY_CHECKS,
    /// FOREIGN_KEY_CHECKS=0`). Engine applies each pair in
    /// source order. Pairs whose LHS is a MySQL session/user
    /// variable (`@VAR` / `@@VAR`) are recorded with the raw
    /// name so the engine can ignore them; pairs whose LHS is
    /// a recognised engine parameter (e.g. `FOREIGN_KEY_CHECKS`)
    /// go through the regular `set_session_param` path.
    SetParameterList(Vec<(String, SetValue)>),
    /// v7.12.1 — `RESET <name>` / `RESET ALL`. Restores parameter
    /// to its default. No-op for parameters SPG does not track.
    ResetParameter(Option<String>),
    /// v7.12.4 — `CREATE [OR REPLACE] FUNCTION name(args) RETURNS
    /// <type> [LANGUAGE <lang>] AS $$ body $$ [LANGUAGE <lang>]`.
    /// v7.12.4 ships `plpgsql` for `RETURNS TRIGGER` bodies (the
    /// CREATE TRIGGER + AFTER/BEFORE row-level pipeline). Other
    /// languages parse but error at exec time with a clear
    /// unsupported message.
    CreateFunction(CreateFunctionStatement),
    /// v7.12.4 — `CREATE [OR REPLACE] TRIGGER name {BEFORE|AFTER}
    /// {INSERT|UPDATE|DELETE} [OR ...] ON tbl FOR EACH ROW
    /// EXECUTE {FUNCTION|PROCEDURE} fn_name()`. STATEMENT-level
    /// triggers and column-list / WHEN clauses are out of scope
    /// for v7.12.4.
    CreateTrigger(CreateTriggerStatement),
    /// v7.12.4 — `DROP TRIGGER [IF EXISTS] name ON tbl`. Silent
    /// no-op when missing if `IF EXISTS` is set.
    DropTrigger {
        name: String,
        table: String,
        if_exists: bool,
    },
    /// v7.12.4 — `DROP FUNCTION [IF EXISTS] name`. Same shape as
    /// DROP TRIGGER but global (no table scope).
    DropFunction {
        name: String,
        if_exists: bool,
    },
    /// v7.17.0 — `CREATE [TEMPORARY] SEQUENCE [IF NOT EXISTS] name
    /// [AS data_type]
    /// [INCREMENT [BY] n]
    /// [MINVALUE n | NO MINVALUE]
    /// [MAXVALUE n | NO MAXVALUE]
    /// [START [WITH] n]
    /// [CACHE n]
    /// [[NO] CYCLE]
    /// [OWNED BY {table.col | NONE}]`.
    /// Closes the round-7+ silent-no-op SEQUENCE story so pg_dump
    /// emits + nextval/currval/setval downstream all work.
    CreateSequence(CreateSequenceStatement),
    /// v7.17.0 — `ALTER SEQUENCE [IF EXISTS] name <options>` with
    /// the same option grammar as CREATE SEQUENCE, plus
    /// `RESTART [WITH n]` and `OWNED BY ...` re-attach.
    AlterSequence(AlterSequenceStatement),
    /// v7.17.0 — `DROP SEQUENCE [IF EXISTS] name [, name…]
    /// [CASCADE | RESTRICT]`. CASCADE / RESTRICT trailers parsed
    /// silently (no FK on sequences).
    DropSequence {
        names: Vec<String>,
        if_exists: bool,
    },
    /// v7.17.0 Phase 1.2 — `CREATE [OR REPLACE] [TEMPORARY] VIEW
    /// [IF NOT EXISTS] name [(col, …)] AS <SELECT …>`. Closes the
    /// silent-no-op VIEW story from the v7.17 customer-readiness
    /// audit: pre-v7.17 SPG parsed CREATE VIEW as Statement::Empty
    /// so any downstream `SELECT FROM v` errored with table-not-
    /// found. The view body is stored verbatim; SELECT FROM <v>
    /// rewrites at exec-time by prepending the view body as a
    /// synthetic CTE.
    CreateView(CreateViewStatement),
    /// v7.17.0 Phase 1.2 — `DROP VIEW [IF EXISTS] name [, name…]
    /// [CASCADE | RESTRICT]`. Removes the matching view from the
    /// catalog; CASCADE/RESTRICT parsed silently.
    DropView {
        names: Vec<String>,
        if_exists: bool,
    },
    /// v7.17.0 Phase 1.3 — `CREATE MATERIALIZED VIEW [IF NOT
    /// EXISTS] name [(col, …)] AS <SELECT …> [WITH [NO] DATA]`.
    /// Closes the silent-no-op MATERIALIZED VIEW story. Storage
    /// model: the materialised result lives as a regular table
    /// with the matching name + a parallel
    /// `materialized_views` registry mapping name → body source
    /// (used by REFRESH).
    CreateMaterializedView(CreateMaterializedViewStatement),
    /// v7.17.0 Phase 1.3 — `REFRESH MATERIALIZED VIEW name [WITH
    /// [NO] DATA]`. Re-runs the stored body and replaces the
    /// cached rows. `WITH NO DATA` truncates without re-running.
    RefreshMaterializedView {
        name: String,
        with_data: bool,
    },
    /// v7.17.0 Phase 1.3 — `DROP MATERIALIZED VIEW [IF EXISTS]
    /// name [, name…] [CASCADE | RESTRICT]`. Drops both the
    /// backing table and the source registry entry.
    DropMaterializedView {
        names: Vec<String>,
        if_exists: bool,
    },
    /// v7.17.0 Phase 1.4 — `CREATE TYPE name AS ENUM ('a', 'b',
    /// …)`. Closes the silent-no-op CREATE TYPE story so PG
    /// dumps that declare enum types load with real constraints
    /// instead of becoming free-form TEXT. Future kinds
    /// (composite / range / domain) extend the inner `kind`
    /// enum.
    CreateType(CreateTypeStatement),
    /// v7.17.0 Phase 1.4 — `DROP TYPE [IF EXISTS] name [, name…]
    /// [CASCADE | RESTRICT]`. Removes the matching enum/domain
    /// from the catalog.
    DropType {
        names: Vec<String>,
        if_exists: bool,
    },
    /// v7.17.0 Phase 1.5 — `CREATE DOMAIN name AS base_type
    /// [DEFAULT expr] [NOT NULL | NULL] [CHECK (expr)]*`.
    /// A DOMAIN is a named CHECK-constrained alias over a built-
    /// in type. The CHECK + NOT NULL + DEFAULT clauses apply to
    /// every column declared with the domain. Closes the
    /// silent-no-op CREATE DOMAIN story so PG dumps that ship
    /// validated identifier types (email, positive_int, …) keep
    /// their guarantees.
    CreateDomain(CreateDomainStatement),
    /// v7.17.0 Phase 1.5 — `DROP DOMAIN [IF EXISTS] name
    /// [, name…] [CASCADE | RESTRICT]`. Removes the matching
    /// domain from the catalog.
    DropDomain {
        names: Vec<String>,
        if_exists: bool,
    },
    /// v7.17.0 Phase 1.6 — `CREATE SCHEMA [IF NOT EXISTS]
    /// name [AUTHORIZATION user]`. SPG is single-database;
    /// schemas are tracked as a namespace registry so pg_dump
    /// multi-schema declarations land cleanly and `SELECT *
    /// FROM information_schema.schemata` returns real entries.
    /// Schema-qualified `schema.table` references still strip
    /// the prefix at lookup time per PG (schemas are not
    /// isolation boundaries in v7.17 — see project-next-docket
    /// for the v7.18+ isolation tracking).
    CreateSchema {
        name: String,
        if_not_exists: bool,
    },
    /// v7.17.0 Phase 1.6 — `DROP SCHEMA [IF EXISTS] name
    /// [, name…] [CASCADE | RESTRICT]`. Removes the schema
    /// from the registry; built-in `public` / `pg_catalog` /
    /// `information_schema` cannot be dropped.
    DropSchema {
        names: Vec<String>,
        if_exists: bool,
    },
}

/// v7.17.0 Phase 1.5 — `CREATE DOMAIN` AST.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateDomainStatement {
    pub name: String,
    /// Base type for the domain (one of the built-in
    /// `ColumnTypeName` variants). User-defined enum / domain
    /// bases are deferred to Phase 1.5b.
    pub base_type: ColumnTypeName,
    /// Optional `DEFAULT <expr>`. Resolved at engine-side
    /// CREATE TABLE time when a column is bound to this domain.
    pub default: Option<Expr>,
    /// `NOT NULL` from the domain definition. Engine ORs this
    /// with the column-level nullability so the strictest of the
    /// two wins (i.e. the column is non-nullable if either side
    /// says so).
    pub not_null: bool,
    /// Zero-or-more `CHECK (expr)` predicates. Each one is
    /// enforced as part of the column's CHECK list at INSERT /
    /// UPDATE time, with `VALUE` substituted for the column's
    /// current cell value.
    pub checks: Vec<Expr>,
}

/// v7.17.0 Phase 1.4 — `CREATE TYPE` AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTypeStatement {
    pub name: String,
    pub kind: TypeKind,
}

/// v7.17.0 Phase 1.4 — flavour of the new type. Only ENUM is
/// implemented; the variant set is open so Phase 1.5 (DOMAIN)
/// and later (COMPOSITE, RANGE) can land without an AST shape
/// migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    /// `AS ENUM ('a', 'b', …)`. Order is preserved (PG enum
    /// labels are ordered).
    Enum { labels: Vec<String> },
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

/// v7.17.0 — `CREATE SEQUENCE` AST node. See [`Statement::CreateSequence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSequenceStatement {
    pub name: String,
    pub if_not_exists: bool,
    pub temporary: bool,
    /// Optional `AS data_type`. Default in PG is BIGINT; SPG matches.
    pub data_type: Option<SequenceDataType>,
    pub options: SequenceOptions,
}

/// v7.17.0 — narrow type for `AS` clause of CREATE SEQUENCE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceDataType {
    SmallInt,
    Int,
    BigInt,
}

/// v7.17.0 — option grammar shared by CREATE / ALTER SEQUENCE.
/// All fields are optional. `min_value`/`max_value` carry
/// `Some(SeqBound::NoBound)` for `NO MINVALUE` / `NO MAXVALUE`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SequenceOptions {
    pub increment: Option<i64>,
    pub min_value: Option<SeqBound>,
    pub max_value: Option<SeqBound>,
    pub start: Option<i64>,
    /// `RESTART [WITH n]` — ALTER-only. `Some(None)` = bare
    /// RESTART, `Some(Some(n))` = RESTART WITH n.
    pub restart: Option<Option<i64>>,
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
    pub owned_by: Option<SequenceOwnedBy>,
}

/// v7.17.0 — `MINVALUE n` / `NO MINVALUE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqBound {
    Value(i64),
    NoBound,
}

/// v7.17.0 — `OWNED BY {table.col | NONE}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceOwnedBy {
    None,
    Column { table: String, column: String },
}

/// v7.17.0 Phase 1.3 — `CREATE MATERIALIZED VIEW` AST node.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateMaterializedViewStatement {
    pub name: String,
    pub if_not_exists: bool,
    /// Optional `(col, col, …)` rename list. Applies to the
    /// backing table at CREATE / REFRESH time.
    pub columns: Vec<String>,
    /// Underlying SELECT. Re-parsed at REFRESH time to rebuild
    /// the cached rows.
    pub body: SelectStatement,
    /// `WITH DATA` (default) = materialise the rows at CREATE
    /// time. `WITH NO DATA` = create an empty backing table;
    /// callers must REFRESH before SELECT returns rows.
    pub with_data: bool,
}

/// v7.17.0 Phase 1.2 — `CREATE VIEW` AST node.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateViewStatement {
    pub name: String,
    pub or_replace: bool,
    pub if_not_exists: bool,
    pub temporary: bool,
    /// Optional `(col, col, …)` rename list. When non-empty,
    /// these override the body's projected column names per-
    /// position at SELECT-from-view time.
    pub columns: Vec<String>,
    /// Underlying SELECT. Re-parsed lazily at SELECT-from-view
    /// time to materialise the view as a synthetic CTE.
    pub body: SelectStatement,
}

/// v7.17.0 — `ALTER SEQUENCE` AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterSequenceStatement {
    pub name: String,
    pub if_exists: bool,
    pub options: SequenceOptions,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterIndexTarget {
    /// `REBUILD [WITH (encoding = <enc>)]`. `encoding = None`
    /// rebuilds the existing graph in place without touching the
    /// column encoding; `Some(enc)` re-encodes every cell first.
    Rebuild { encoding: Option<VecEncoding> },
    /// v7.16.2 — `[IF EXISTS] RENAME TO <new>`. mailrs migrate-042
    /// uses this; PG drops the IF EXISTS noisily as ERROR, mailrs
    /// uses it to make the migration idempotent (re-running on a
    /// DB where the rename already happened is a no-op rather
    /// than an error).
    Rename { new: String, if_exists: bool },
}

/// v6.7.2 — `ALTER TABLE t SET <setting> = <value>`. v6.7.2 ships
/// the single `hot_tier_bytes` setting; later v6.7.x sub-versions
/// can add more SET subjects without changing the dispatch shape.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTableStatement {
    pub name: String,
    /// v7.13.2 — mailrs round-6 S1. One or more subactions
    /// separated by commas in the source SQL. PG-semantic apply
    /// is sequential; engine bails on first error (no
    /// transactional rollback of completed subactions in v7.13).
    /// Single-subaction shape stays a 1-element vec.
    pub targets: Vec<AlterTableTarget>,
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
    /// v7.6.8 — `ALTER TABLE t DROP CONSTRAINT [IF EXISTS] name`.
    /// `if_exists` (v7.13.2 mailrs round-6 S7) makes the drop a
    /// no-op when no FK with that name exists; otherwise raises.
    DropForeignKey { name: String, if_exists: bool },
    /// v7.13.0 — `ALTER TABLE t ADD [COLUMN] [IF NOT EXISTS] <col>
    /// <type> [DEFAULT <expr>] [NOT NULL]`. mailrs round-5 G1
    /// (20 migrate-*.sql hits). Engine appends the column to the
    /// schema and back-fills every existing row with the DEFAULT
    /// (or NULL when no DEFAULT and the column is nullable).
    AddColumn {
        column: ColumnDef,
        if_not_exists: bool,
    },
    /// v7.13.0 — `ALTER TABLE t ALTER COLUMN <col> TYPE <ty>
    /// [USING <expr>]` (mailrs round-5 G8). Engine rewrites every
    /// existing row's column value by evaluating the optional
    /// USING expression (default `col::<ty>`) and re-coercing
    /// against the new column type.
    AlterColumnType {
        column: String,
        new_type: ColumnTypeName,
        using: Option<Expr>,
    },
    /// v7.13.3 — `ALTER TABLE t DROP [COLUMN] [IF EXISTS] <col>
    /// [CASCADE | RESTRICT]` (mailrs round-7 S8). The column +
    /// every row's value at that position is removed; any index
    /// on the column is dropped. `if_exists` makes the drop a
    /// no-op when the column is missing. `cascade` removes
    /// dependents (FKs referencing the column, partial indexes
    /// whose predicate names the column); without it, the engine
    /// rejects when dependents exist.
    DropColumn {
        column: String,
        if_exists: bool,
        cascade: bool,
    },
    /// v7.14.0 — `ALTER TABLE t ADD CONSTRAINT name PRIMARY KEY
    /// (cols)` / `ADD CONSTRAINT name UNIQUE (cols)` / `ADD
    /// CONSTRAINT name CHECK (expr)` — table-level constraints
    /// installed post-CREATE-TABLE. pg_dump emits PKs as a
    /// separate ALTER TABLE statement, so this surface lets the
    /// dump load straight through.
    AddTableConstraint(TableConstraint),
    /// v7.15.0 — `ALTER TABLE t RENAME [COLUMN] old TO new`.
    /// Renames the column in the schema and propagates the rename
    /// to every stored source string that references it as a
    /// (potentially-qualified) column identifier: CHECK predicates,
    /// partial-index predicates, runtime DEFAULT expressions, and
    /// triggers' `UPDATE OF` column lists. Function bodies and
    /// trigger bodies are NOT auto-rewritten — they're loose
    /// source text and may contain references SPG can't statically
    /// resolve to this column (NEW./OLD. + dynamic SQL). Renames
    /// the column even if dependents exist; users renaming a
    /// column referenced by a function body update the function
    /// body separately.
    RenameColumn { old: String, new: String },
    /// v7.16.2 — `ALTER TABLE old RENAME TO new`. Renames the
    /// table itself (mailrs round-10 A.5 carve-out — mailrs's
    /// migrate-042 uses it). The engine moves the table entry
    /// in the catalog under the new name; child catalog state
    /// (FKs pointing at this table, triggers watching this
    /// table) tracks the rename through the storage layer.
    RenameTable { new: String },
    /// v7.16.1 — `ALTER TABLE t { ENABLE | DISABLE } TRIGGER
    /// { ALL | <name> }`. Toggles whether row-level triggers
    /// fire on subsequent INSERT/UPDATE/DELETE on the table.
    /// `pg_dump --disable-triggers` emits a DISABLE wrapper +
    /// ENABLE epilogue around every table's data block so the
    /// rows already-computed in prod don't get re-rewritten
    /// (and so trigger-driven side effects like
    /// audit/queueing don't re-fire during a bulk reload).
    /// `which == TriggerSelector::All` toggles every trigger
    /// on the table; `Named(name)` toggles one trigger. The
    /// engine persists the disabled state on `TriggerDef.enabled`
    /// (catalog FILE_VERSION 25+) and the row-write paths skip
    /// the trigger when `!enabled`.
    SetTriggerEnabled {
        which: TriggerSelector,
        enabled: bool,
    },
}

/// v7.16.1 — target of `ALTER TABLE … { ENABLE | DISABLE }
/// TRIGGER …`. PG also accepts `USER`, `REPLICA`, `ALWAYS`
/// modifiers; v7.16.1 ships the two shapes pg_dump actually
/// emits (`ALL` + per-name) — the rest parse-accept as `Named`
/// shouldn't surface from a dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerSelector {
    /// Every trigger on the table.
    All,
    /// A specific trigger by name.
    Named(String),
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

/// v7.12.4 — `CREATE [OR REPLACE] FUNCTION`. v7.12.4 ships
/// `RETURNS TRIGGER LANGUAGE plpgsql` as the primary use case
/// (the row-level trigger body the CREATE TRIGGER below references).
/// Non-trigger user-defined functions parse but error at execution
/// time with a clear unsupported message; that surface lands in
/// v7.12.5+.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateFunctionStatement {
    pub name: String,
    /// `OR REPLACE` was present; an existing function with the
    /// same name is overwritten instead of erroring.
    pub or_replace: bool,
    /// `(arg1 type1, ...)` — v7.12.4 only accepts the empty arg
    /// list `()` (sufficient for trigger functions). Other shapes
    /// parse and store the args but the executor refuses to call
    /// them.
    pub args: Vec<FunctionArg>,
    /// `RETURNS <type>` — `trigger` is the supported shape for
    /// v7.12.4; arbitrary return types parse to
    /// [`FunctionReturn::Other`].
    pub returns: FunctionReturn,
    /// `LANGUAGE <lang>` clause. PG accepts the clause on either
    /// side of `AS $$...$$`; the parser canonicalises to one slot.
    /// `plpgsql` and `sql` are the two interesting values.
    pub language: String,
    /// `AS $$ ... $$` body. v7.12.4 parses PL/pgSQL bodies into
    /// a structured AST; non-trigger / non-plpgsql bodies stay as
    /// the raw source text so the v7.12.5+ executor can pick them
    /// up without a parser rev.
    pub body: FunctionBody,
}

/// v7.12.4 — one positional argument to a `CREATE FUNCTION`.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionArg {
    /// `IN` / `OUT` / `INOUT` mode. v7.12.4 only accepts `IN`
    /// (the default); `OUT` / `INOUT` parse but the executor
    /// refuses them.
    pub mode: FunctionArgMode,
    /// Optional arg name. Trigger functions traditionally don't
    /// name their args (they read NEW/OLD instead), so `None` is
    /// the common case.
    pub name: Option<String>,
    /// Declared type, normalised to the SPG `DataType` mapping
    /// where one exists. Unknown / extension types parse as a
    /// raw string under [`FunctionArgType::Raw`].
    pub ty: FunctionArgType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionArgMode {
    In,
    Out,
    InOut,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArgType {
    Typed(ColumnTypeName),
    /// Unknown / extension types — kept as the parser-side raw
    /// identifier so error messages can name them precisely.
    Raw(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionReturn {
    /// `RETURNS TRIGGER` — the row-level trigger function shape.
    /// v7.12.4 ships exactly this for execution.
    Trigger,
    /// `RETURNS VOID`. Parses; executor rejects in v7.12.4 unless
    /// the function is unused (since v7.12.4 doesn't ship scalar
    /// function invocation).
    Void,
    /// `RETURNS <type>` for any concrete data type. Reserved for
    /// v7.12.5+'s scalar UDF surface.
    Type(ColumnTypeName),
    /// `RETURNS <ident>` for types SPG doesn't know — extension
    /// types, RETURNS SETOF rows, RETURNS TABLE(...), etc.
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionBody {
    /// v7.12.4 — parsed PL/pgSQL `BEGIN … END` block. The
    /// trigger-function executor walks this directly without
    /// re-parsing.
    PlPgSql(PlPgSqlBlock),
    /// Raw source text — parser couldn't (or didn't try to)
    /// structure-parse the body. Used for `LANGUAGE sql`
    /// functions and any PL/pgSQL body that contains v7.12.5+
    /// features the v7.12.4 parser doesn't yet recognise. The
    /// executor returns an unsupported error when invoked.
    Raw(String),
}

/// v7.12.4 — PL/pgSQL `BEGIN ... END;` block. v7.12.6 widens
/// from assignment + return to a real-PL/pgSQL surface:
/// `DECLARE`-block local variables, `IF/ELSIF/ELSE/END IF`
/// control flow, `RAISE` diagnostics, and embedded SQL
/// statements that execute through the regular engine path.
/// The remaining v7.12.x carve-out is loops (`LOOP/WHILE/FOR`),
/// which mailrs's trigger doesn't need but other PG customers
/// may; deferred to a future minor release.
#[derive(Debug, Clone, PartialEq)]
pub struct PlPgSqlBlock {
    /// v7.12.6 — `DECLARE var TYPE [:= init_expr];` declarations
    /// preceding `BEGIN`. Empty when the body opens directly with
    /// `BEGIN`. Declarations execute in order; each may reference
    /// earlier-declared locals in its init expression.
    pub declarations: Vec<PlPgSqlDeclare>,
    pub statements: Vec<PlPgSqlStmt>,
}

/// v7.12.6 — single `DECLARE` entry: variable name + declared
/// type + optional initialiser. Variables default to SQL NULL
/// when no init is given (matches PG).
#[derive(Debug, Clone, PartialEq)]
pub struct PlPgSqlDeclare {
    pub name: String,
    /// Declared SQL type (mapped to [`ColumnTypeName`] where SPG
    /// knows it; raw text otherwise).
    pub ty: FunctionArgType,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlPgSqlStmt {
    /// `NEW.col := expr;` or `OLD.col := expr;`. OLD is parsed
    /// for clarity in error reporting (PG also forbids it) — the
    /// executor errors with a clear "OLD is read-only" message.
    Assign { target: AssignTarget, value: Expr },
    /// v7.16.2 — plpgsql `SELECT <projection> INTO <var>
    /// [FROM …]` (mailrs round-10 migrate-042). The `body` is
    /// the SELECT statement with the INTO clause stripped; the
    /// engine runs it via `Engine::execute`, takes the first
    /// row's first column, and assigns to the local variable
    /// in the DECLARE scope. Single-column / single-row
    /// queries only at v7.16.2; multi-target (`INTO a, b`) is
    /// a v7.16.x follow-up.
    SelectInto {
        var: String,
        body: Box<SelectStatement>,
    },
    /// `RETURN <target>;` — trigger functions canonically return
    /// `NEW` / `OLD` / `NULL`; v7.12.4 also accepts a bare
    /// expression for forward compatibility with scalar UDFs.
    Return(ReturnTarget),
    /// v7.12.6 — `IF cond THEN body [ELSIF cond THEN body]*
    /// [ELSE body] END IF;`. Branches are tried in order; first
    /// truthy condition wins; the optional ELSE runs when no
    /// condition matched.
    If {
        branches: Vec<(Expr, Vec<PlPgSqlStmt>)>,
        else_branch: Vec<PlPgSqlStmt>,
    },
    /// v7.12.6 — `RAISE <level> '<fmt>' [, args]*;`. Level is one
    /// of `NOTICE` / `WARNING` / `INFO` / `LOG` / `DEBUG`
    /// (logging — observable side effect only) or `EXCEPTION`
    /// (aborts the trigger and propagates as an error). v7.12.6
    /// supports the basic format-string substitution PG uses
    /// (`%` placeholders consumed positionally).
    Raise {
        level: RaiseLevel,
        message: String,
        args: Vec<Expr>,
    },
    /// v7.12.6 — embedded SQL statement inside the trigger body
    /// (`INSERT INTO …`, `UPDATE …`, `DELETE FROM …`, `SELECT …`).
    /// NEW.col / OLD.col references inside the embedded
    /// statement's expression tree are substituted with the
    /// current trigger context before the engine re-executes the
    /// statement. Recursion depth into nested triggers is
    /// bounded by the engine's existing trigger-fire guard.
    EmbeddedSql(Box<Statement>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseLevel {
    /// `RAISE NOTICE` — diagnostic message, observable in the
    /// server log. Does not affect the trigger's outcome.
    Notice,
    /// `RAISE WARNING` — like NOTICE, slightly louder severity.
    Warning,
    /// `RAISE INFO` — like NOTICE, slightly quieter.
    Info,
    /// `RAISE LOG` — like NOTICE, lower priority.
    Log,
    /// `RAISE DEBUG` — like NOTICE, lowest priority.
    Debug,
    /// `RAISE EXCEPTION` — aborts the trigger function with the
    /// given message, propagating up to the caller as a query-
    /// level error.
    Exception,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssignTarget {
    NewColumn(String),
    OldColumn(String),
    /// Reserved for v7.12.5 DECLARE'd local variables.
    Local(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReturnTarget {
    /// `RETURN NEW;` — for BEFORE triggers, this is the row that
    /// actually gets written (possibly with NEW.col mutations
    /// applied). For AFTER triggers, the return value is ignored.
    New,
    /// `RETURN OLD;` — pass-through. For BEFORE DELETE this lets
    /// the delete proceed; for BEFORE UPDATE / INSERT it's
    /// equivalent to dropping the write.
    Old,
    /// `RETURN NULL;` — for BEFORE triggers, skips the write
    /// entirely. For AFTER, the return value is ignored.
    Null,
    /// `RETURN <expr>;` — non-row return shape; reserved for the
    /// scalar UDF surface in v7.12.5+. Executor errors when used
    /// inside a trigger function.
    Expr(Expr),
}

/// v7.12.4 — `CREATE [OR REPLACE] TRIGGER`. Always row-level
/// (`FOR EACH ROW`) in v7.12.4 — statement-level triggers parse
/// but the executor refuses them. `WHEN (cond)` clauses are out
/// of scope; the trigger function can short-circuit on a leading
/// IF inside its body once v7.12.5 lands IF.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTriggerStatement {
    pub name: String,
    pub or_replace: bool,
    pub timing: TriggerTiming,
    /// At least one event; `INSERT OR UPDATE OR DELETE` parses to
    /// three entries in order.
    pub events: Vec<TriggerEvent>,
    pub table: String,
    /// `FOR EACH ROW` vs `FOR EACH STATEMENT`. v7.12.4 ships
    /// only `Row`; `Statement` parses but the executor refuses.
    pub for_each: TriggerForEach,
    /// Name of the function to invoke. v7.12.4 requires the
    /// function to be `CREATE FUNCTION`'d earlier; forward
    /// references (PG accepts) are deferred to v7.12.5.
    pub function: String,
    /// v7.13.0 — `UPDATE OF col, col, …` column-list filter
    /// (mailrs round-5 G7). Non-empty only when the events list
    /// contains UPDATE and the user wrote the column-list filter.
    /// PG fires the trigger only when at least one of these
    /// columns appears in the SET clause; SPG conservatively
    /// fires on any UPDATE matching the listed columns or
    /// rewriting them at the row level. Empty vec = no filter
    /// (fire on every UPDATE).
    pub update_columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    /// Fires before the row is written; the trigger function's
    /// return value (NEW or NULL) decides the row content and
    /// whether the write proceeds at all.
    Before,
    /// Fires after the row is written; the return value is
    /// ignored.
    After,
    /// `INSTEAD OF` is PG-VIEW-trigger-only and out of scope for
    /// v7.12.4 (SPG has no updatable-view surface).
    InsteadOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
    /// `TRUNCATE` event parses; SPG has no TRUNCATE statement
    /// so the trigger never fires.
    Truncate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerForEach {
    Row,
    Statement,
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
    /// v7.15.0 — operator class on the leading column, when the
    /// CREATE INDEX named one (`(col vector_cosine_ops)` shape).
    /// Lower-cased. Most opclasses are still informational; the
    /// engine routes on `gin_trgm_ops` specifically to build a
    /// trigram-shingle GIN over a TEXT column, and otherwise
    /// keeps the current "accepted and discarded" behaviour for
    /// pg_dump compatibility.
    pub opclass: Option<String>,
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
        /// v7.13.0 — `NULLS NOT DISTINCT` modifier (mailrs round-5
        /// G10). PG 15+ flips the NULL handling so any number of
        /// NULL rows collide on the constraint. Default is
        /// `false` (NULLS DISTINCT, standard SQL behaviour).
        nulls_not_distinct: bool,
    },
    /// v7.13.0 — `CHECK (<expr>)` table-level constraint
    /// (mailrs round-5 G3). Column-level inline CHECKs fold into
    /// this same variant at parse time. Engine evaluates the
    /// predicate against each INSERT/UPDATE candidate row; a
    /// false / NULL result rejects the mutation.
    Check { name: Option<String>, expr: Expr },
    /// v7.15.0 — MySQL `KEY name (cols)` / `INDEX name (cols)`
    /// non-unique secondary-index declaration inline in CREATE
    /// TABLE. Engine builds a BTree index on the leading column
    /// (composite columns parse but only the leading column is
    /// honoured at v7.15 — matches the existing
    /// `CreateIndexStatement::extra_columns` semantics). Useful
    /// for `mysql/blog`-style schemas that lean on routine
    /// secondary indexes for ORM lookups.
    Index {
        name: Option<String>,
        columns: Vec<String>,
    },
    /// v7.17.0 Phase 2.2 — MySQL `FULLTEXT KEY/INDEX [name]
    /// (cols)` inline declaration. Pre-v7.17 the parser
    /// silently dropped these so MyISAM-imported FULLTEXT
    /// indexes vanished; v7.17 routes them through the
    /// existing tsvector-GIN engine path so MATCH AGAINST
    /// queries get a real inverted index instead of falling
    /// back to a full scan. Multi-column FULLTEXT KEYs build
    /// one GIN per column at v7.17 (per-column posting lists);
    /// the leading column drives query planning.
    FulltextIndex {
        name: Option<String>,
        columns: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // grammar-driven; each flag maps to a distinct PG column-constraint keyword
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
    /// v7.13.0 — inline `UNIQUE` column constraint
    /// (mailrs round-5 G2). The CREATE TABLE handler folds this
    /// into a single-column `TableConstraint::Unique` so the
    /// engine path stays uniform with table-level UNIQUE.
    pub is_unique: bool,
    /// v7.13.0 — inline `CHECK (<expr>)` column constraint
    /// (mailrs round-5 G3). Stored alongside the column so the
    /// CREATE TABLE handler can fold these into table-level
    /// CHECK constraints. Multiple inline CHECKs on the same
    /// column are concatenated with AND at the table level.
    pub check: Option<Expr>,
    /// v7.17.0 Phase 1.4 — user-defined type reference. When the
    /// parser sees an unknown column-type ident (anything not in
    /// the built-in `parse_column_type_name` table), it sets
    /// `ty = ColumnTypeName::Text` and records the original name
    /// here. The engine resolves at CREATE TABLE time: if a
    /// catalog enum/domain with this name exists, the column is
    /// bound to it (label-checked on INSERT for enums; CHECK-
    /// constrained for domains); otherwise the CREATE TABLE
    /// errors with "unknown type".
    pub user_type_ref: Option<String>,
    /// v7.17.0 Phase 2.1 — MySQL-style `ON UPDATE
    /// CURRENT_TIMESTAMP` column attribute. When set, an
    /// UPDATE that does NOT explicitly bind this column
    /// overrides the new value with `now()` (engine clock).
    /// Pre-v7.17 SPG silently accepted the syntax and never
    /// fired the override — `updated_at` columns from mysqldump
    /// stayed pinned at their initial DEFAULT forever, an
    /// audit Tier-S silent-failure. Generalised as a stored
    /// expression source so future shapes (`ON UPDATE
    /// CURRENT_TIMESTAMP(6)`, `ON UPDATE LOCALTIMESTAMP`) reuse
    /// the same field; v7.17 only accepts CURRENT_TIMESTAMP.
    pub on_update_runtime: Option<Expr>,
    /// v7.17.0 Phase 2.5 — text collation derived from the
    /// post-fix `COLLATE <name>` clause (and / or the table-level
    /// `COLLATE=<name>` for MySQL dumps that don't repeat it
    /// per column). Pre-2.5 SPG accepted the clause and
    /// discarded the name, leaving every column byte-compared
    /// — a Tier-S silent failure when the customer expected
    /// `_ci` / `case_insensitive` semantics. Parser normalises
    /// the raw collation name into the variants in `Collation`.
    /// Default `Binary` preserves the legacy compare path.
    pub collation: Collation,
    /// v7.17.0 Phase 4.4 — MySQL `UNSIGNED` modifier flag. Pre-
    /// 4.4 SPG accepted and discarded the keyword, leaving
    /// negative values silently accepted on a column the
    /// customer declared `INT UNSIGNED NOT NULL`. Now: the engine
    /// rejects negative INSERT / UPDATE values on UNSIGNED int
    /// columns. SPG widening to `u64`-shaped storage is out of
    /// v7.17 scope; the upper bound remains the signed-type max
    /// (i64::MAX for BIGINT UNSIGNED), which still strictly
    /// exceeds what every mailrs / Rails app actually uses.
    pub is_unsigned: bool,
    /// v7.17.0 Phase 3.P0-36 — MySQL inline `ENUM('a','b','c')`
    /// value list captured at parse time. When `Some`, the parser
    /// recognised `ENUM(...)` in the type slot; the engine
    /// validates INSERT cells against this list at
    /// column_def_to_schema time and persists the variants on
    /// `ColumnSchema.inline_enum_variants`. None for all
    /// non-ENUM columns.
    pub inline_enum_variants: Option<Vec<String>>,
    /// v7.17.0 Phase 3.P0-37 — MySQL inline `SET('a','b','c')`
    /// value list. Distinct from ENUM (subset semantics rather
    /// than pick-one). None for all non-SET columns.
    pub inline_set_variants: Option<Vec<String>>,
}

/// v7.17.0 Phase 2.5 — text collation classification surfaced
/// from the SQL parser. Mirrors `spg_storage::Collation`; the
/// engine bridges between the two at CREATE TABLE time.
///
/// Recognised collation-name patterns (case-insensitive):
///   * `case_insensitive`, `*_ci`, `*_ai_ci`, `nocase`         → CaseInsensitive
///   * Everything else (`C`, `POSIX`, `default`,
///     `pg_catalog.default`, `*_cs`, `*_bin`, unknown names)   → Binary
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collation {
    Binary,
    CaseInsensitive,
}

impl Default for Collation {
    fn default() -> Self {
        Self::Binary
    }
}

impl Collation {
    /// Classify a `COLLATE <name>` ident into one of the supported
    /// variants. Empty / unknown names fall back to `Binary` —
    /// matches the pre-2.5 silent-accept behaviour for snapshots
    /// that load through but don't actually depend on the
    /// collation semantics.
    #[must_use]
    pub fn from_collation_name(name: &str) -> Self {
        let lc = name.trim().to_ascii_lowercase();
        // Strip any quotes / schema-qualifier the parser left on
        // (e.g. `pg_catalog.default`).
        let bare = lc
            .trim_matches(|c: char| c == '"' || c == '\'')
            .rsplit('.')
            .next()
            .unwrap_or("");
        if bare.is_empty() {
            return Self::Binary;
        }
        if bare == "case_insensitive" || bare == "nocase" {
            return Self::CaseInsensitive;
        }
        // MySQL `_ci` suffix (covers `utf8mb4_general_ci`,
        // `utf8mb4_unicode_ci`, `utf8mb4_0900_ai_ci`, …).
        if bare.ends_with("_ci") {
            return Self::CaseInsensitive;
        }
        Self::Binary
    }
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
    /// v7.17.0 `UUID` — 128-bit identifier. PG wire OID 2950.
    /// Literal input accepts canonical hyphenated, unhyphenated,
    /// uppercase, and `{...}`-braced forms; display normalises to
    /// canonical lowercase 8-4-4-4-12. The drop-in PG surface for
    /// Django / Rails / Hibernate `id UUID PRIMARY KEY DEFAULT
    /// gen_random_uuid()`.
    Uuid,
    /// v7.17.0 Phase 3.P0-32 `TIME` (without time zone) — i64
    /// microseconds since 00:00:00. PG wire OID 1083. Literal
    /// input is `'HH:MM:SS'` with an optional `.fraction` suffix
    /// (6-digit microsecond precision). Display normalises to
    /// the canonical `HH:MM:SS[.ffffff]`.
    Time,
    /// v7.17.0 Phase 3.P0-33 MySQL `YEAR` — u16 in range
    /// 1901..=2155 plus the zero-year sentinel 0. No dedicated
    /// PG OID; advertised as INT4 on the wire. Display always
    /// 4 digits zero-padded.
    Year,
    /// v7.17.0 Phase 3.P0-34 PG `TIME WITH TIME ZONE` (TIMETZ) —
    /// i64 us since 00:00:00 (local) + i32 offset_secs from UTC.
    /// Wire OID 1266. Literal input is `'HH:MM:SS[.ffffff]±HH[:MM]'`.
    /// Offset range: ±14 hours.
    TimeTz,
    /// v7.17.0 Phase 3.P0-35 PG `MONEY` — i64 cents
    /// (locale-independent storage). Wire OID 790. Literal input
    /// accepts `$N.NN`, `$N,NNN.NN`, bare integer (treated as
    /// major units), optional leading `-`. Display: en_US locale.
    Money,
    /// v7.17.0 Phase 3.P0-38 PG range types. Pair stores the
    /// element kind tag (Int4 / Int8 / Num / Ts / TsTz / Date)
    /// — the engine bridges to `DataType::Range(RangeKind)`.
    Range(RangeKindAst),
    /// v7.17.0 Phase 3.P0-39 PG `hstore` extension type — flat
    /// `text => text` map with NULL value support.
    Hstore,
    /// v7.17.0 Phase 3.P0-40 — 2D arrays for INT / TEXT / BIGINT.
    IntArray2D,
    BigIntArray2D,
    TextArray2D,
}

/// v7.17.0 Phase 3.P0-38 — PG range element kind. Mirrors
/// `spg_storage::RangeKind`; we keep it spg-sql-local so the AST
/// crate doesn't depend on storage. Bridged at engine boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RangeKindAst {
    Int4,
    Int8,
    Num,
    Ts,
    TsTz,
    Date,
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
            Self::Uuid => f.write_str("UUID"),
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
            Self::Time => f.write_str("TIME"),
            Self::Year => f.write_str("YEAR"),
            Self::TimeTz => f.write_str("TIMETZ"),
            Self::Money => f.write_str("MONEY"),
            Self::Range(k) => f.write_str(match k {
                RangeKindAst::Int4 => "INT4RANGE",
                RangeKindAst::Int8 => "INT8RANGE",
                RangeKindAst::Num => "NUMRANGE",
                RangeKindAst::Ts => "TSRANGE",
                RangeKindAst::TsTz => "TSTZRANGE",
                RangeKindAst::Date => "DATERANGE",
            }),
            Self::Hstore => f.write_str("HSTORE"),
            Self::IntArray2D => f.write_str("INT[][]"),
            Self::BigIntArray2D => f.write_str("BIGINT[][]"),
            Self::TextArray2D => f.write_str("TEXT[][]"),
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

/// v7.17.0 Phase 3.P0-42 — SQL:2003 / PG 15+ MERGE statement.
/// One WHEN clause fires per source row depending on whether the
/// `on` condition matched any target row(s); the executor walks
/// `clauses` in declaration order and fires the first whose
/// `matched` kind and optional `condition` are both satisfied.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeStatement {
    pub target: String,
    pub target_alias: Option<String>,
    pub source: String,
    pub source_alias: Option<String>,
    pub on: Expr,
    pub clauses: Vec<MergeWhenClause>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MergeWhenClause {
    pub matched: MergeMatched,
    /// Optional `AND <expr>` filter — when present, the clause
    /// only fires for the source rows whose match-pair satisfies
    /// the predicate.
    pub condition: Option<Expr>,
    pub action: MergeAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMatched {
    Matched,
    NotMatched,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MergeAction {
    /// `INSERT (cols) VALUES (vals)`. SPG v7.17 requires the
    /// explicit column list (the bare `INSERT VALUES (vals)`
    /// shape lands later).
    Insert {
        columns: Vec<String>,
        values: Vec<Expr>,
    },
    /// `UPDATE SET col = expr [, …]` — applied to every matched
    /// target row for the firing source row.
    Update { assignments: Vec<(String, Expr)> },
    /// `DELETE` — drop every matched target row.
    Delete,
    /// `DO NOTHING` — explicit no-op (the SQL standard accepts
    /// the clause and SPG mirrors so a customer-side MERGE that
    /// uses it for branch-control doesn't error).
    DoNothing,
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
    /// v1.3+ accepts `INSERT INTO t VALUES (a), (b)`. Empty when
    /// `select_source` is `Some` (the engine builds rows from the
    /// inner SELECT result set instead).
    pub rows: Vec<Vec<Expr>>,
    /// v7.13.0 — `INSERT INTO t [(cols)] SELECT …` (mailrs
    /// round-5 G4). When present, `rows` is empty and the engine
    /// materialises the SELECT result, coerces each output tuple to
    /// the target column types, and inserts as a single batch.
    pub select_source: Option<Box<SelectStatement>>,
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
    /// v7.17.0 Phase 3.P0-49 — `FETCH FIRST <n> ROWS WITH TIES`
    /// (SQL:2008). When true and an ORDER BY is present, the
    /// executor extends past the LIMIT-truncated tail to include
    /// every row whose ORDER BY key equals the last-kept row's
    /// key. Requires an ORDER BY; the executor errors otherwise
    /// (matching PG's `WITH TIES` rule). The parser was already
    /// accepting `WITH TIES` since Phase 5.1; this field captures
    /// the choice so the executor can act on it.
    pub limit_with_ties: bool,
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
    /// NULL elements become NULL cells. v7.11 supported
    /// uncorrelated UNNEST only as the FROM primary; v7.13.2
    /// (mailrs round-6 S5) widens to UNNEST in any FROM-list
    /// position (cross-join with regular tables).
    pub unnest_expr: Option<Box<Expr>>,
    /// v7.13.2 — mailrs round-6 S5. PG-standard
    /// `UNNEST(<arr>) AS alias(col_name)` column-list aliasing:
    /// when non-empty, the first entry overrides the projected
    /// column name for the unnested column. Empty = fall back to
    /// the table alias (pre-v7.13.2 behaviour).
    pub unnest_column_aliases: Vec<String>,
    /// v7.17.0 Phase 3.10 — `FROM generate_series(start, stop
    /// [, step])` set-returning source. When `Some`, the engine
    /// materialises a single-column virtual table by stepping
    /// `start` to `stop` inclusive. Args are the literal arg list
    /// (2 for default-step, 3 for explicit-step). Supports:
    ///   * SmallInt / Int / BigInt with integer step (default = 1)
    ///   * Timestamp with INTERVAL step (PG date-range pattern)
    /// Mutually exclusive with `unnest_expr` — both populate the
    /// same downstream dispatch slot. `name` defaults to
    /// `"generate_series"` when no alias is provided.
    pub generate_series_args: Option<Vec<Expr>>,
    /// v7.17.0 Phase 3.P0-41 — `LATERAL ( SELECT … )` derived
    /// table. When `Some`, the TableRef is a parenthesised SELECT
    /// that may reference columns from the preceding FROM items
    /// (correlated derived table). The executor materialises the
    /// subquery per left-row, substituting outer-column references
    /// against the current join row's values before running the
    /// inner SELECT, then cross-joins the result back.
    /// Mutually exclusive with `name` / `unnest_expr` /
    /// `generate_series_args`.
    pub lateral_subquery: Option<Box<SelectStatement>>,
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
    /// v7.13.0 — `CASE WHEN <cond> THEN <val> ... ELSE <val> END`
    /// (searched form, `operand` is None) and
    /// `CASE <expr> WHEN <val> THEN <val> ... END` (simple form,
    /// `operand` is the lead expression compared against each
    /// branch's match). Each `(when_expr, then_expr)` branch
    /// stays as written; engine short-circuits on the first match.
    /// `else_branch` is `None` when no ELSE; evaluates to NULL.
    /// mailrs round-5 G9.
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        else_branch: Option<Box<Expr>>,
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
    /// v7.17.0 — `::uuid`. Decodes the LHS Text via
    /// `spg_storage::parse_uuid_str` (accepts canonical hyphenated,
    /// unhyphenated, uppercase, and brace-wrapped forms); malformed
    /// input is a SQL error.
    Uuid,
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
            Self::Uuid => "uuid",
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
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR strict contained-in
    /// `<<`. LHS network is strictly inside RHS network (no equality).
    InetContainedBy,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR contained-in-or-equal
    /// `<<=`. LHS network ⊆ RHS network.
    InetContainedByEq,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR strict contains `>>`.
    /// LHS network strictly contains RHS network.
    InetContains,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR contains-or-equal `>>=`.
    /// LHS network ⊇ RHS network.
    InetContainsEq,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR network overlap `&&`.
    /// True iff either network contains any address of the other.
    InetOverlap,
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
            Self::Empty => Ok(()),
            Self::DropTable { names, if_exists } => {
                f.write_str("DROP TABLE ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", quote_ident(n))?;
                }
                Ok(())
            }
            Self::DropIndex { name, if_exists } => {
                f.write_str("DROP INDEX ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                write!(f, "{}", quote_ident(name))
            }
            Self::Select(s) => s.fmt(f),
            Self::CreateTable(s) => s.fmt(f),
            Self::CreateIndex(s) => s.fmt(f),
            Self::Insert(s) => s.fmt(f),
            Self::Update(s) => s.fmt(f),
            Self::Delete(s) => s.fmt(f),
            Self::Merge(s) => {
                // v7.17.0 Phase 3.P0-42 — MERGE display is approximate
                // (it round-trips for the cases tests cover, not for
                // round-tripping every edge of the surface).
                f.write_str("MERGE INTO ")?;
                write!(f, "{}", quote_ident(&s.target))?;
                if let Some(a) = &s.target_alias {
                    write!(f, " {}", quote_ident(a))?;
                }
                f.write_str(" USING ")?;
                write!(f, "{}", quote_ident(&s.source))?;
                if let Some(a) = &s.source_alias {
                    write!(f, " {}", quote_ident(a))?;
                }
                write!(f, " ON {}", s.on)?;
                for clause in &s.clauses {
                    f.write_str(" WHEN ")?;
                    f.write_str(match clause.matched {
                        MergeMatched::Matched => "MATCHED",
                        MergeMatched::NotMatched => "NOT MATCHED",
                    })?;
                    if let Some(c) = &clause.condition {
                        write!(f, " AND {c}")?;
                    }
                    f.write_str(" THEN ")?;
                    match &clause.action {
                        MergeAction::Insert { columns, values } => {
                            f.write_str("INSERT (")?;
                            for (i, c) in columns.iter().enumerate() {
                                if i > 0 {
                                    f.write_str(", ")?;
                                }
                                write!(f, "{}", quote_ident(c))?;
                            }
                            f.write_str(") VALUES (")?;
                            for (i, v) in values.iter().enumerate() {
                                if i > 0 {
                                    f.write_str(", ")?;
                                }
                                write!(f, "{v}")?;
                            }
                            f.write_str(")")?;
                        }
                        MergeAction::Update { assignments } => {
                            f.write_str("UPDATE SET ")?;
                            for (i, (c, e)) in assignments.iter().enumerate() {
                                if i > 0 {
                                    f.write_str(", ")?;
                                }
                                write!(f, "{} = {e}", quote_ident(c))?;
                            }
                        }
                        MergeAction::Delete => f.write_str("DELETE")?,
                        MergeAction::DoNothing => f.write_str("DO NOTHING")?,
                    }
                }
                Ok(())
            }
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
                write!(f, "ALTER INDEX ")?;
                match &a.target {
                    AlterIndexTarget::Rebuild { encoding } => {
                        write!(f, "{} REBUILD", quote_ident(&a.name))?;
                        if let Some(enc) = encoding {
                            write!(f, " WITH (encoding = {enc})")?;
                        }
                        Ok(())
                    }
                    AlterIndexTarget::Rename { new, if_exists } => {
                        if *if_exists {
                            f.write_str("IF EXISTS ")?;
                        }
                        write!(f, "{} RENAME TO {}", quote_ident(&a.name), quote_ident(new))
                    }
                }
            }
            Self::AlterTable(a) => {
                write!(f, "ALTER TABLE {} ", quote_ident(&a.name))?;
                for (i, t) in a.targets.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    fmt_alter_target(f, t)?;
                }
                Ok(())
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
            Self::DoBlock(body) => write!(f, "DO $$ {body} $$"),
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
            Self::SetParameterList(pairs) => {
                f.write_str("SET ")?;
                for (i, (name, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name} = ")?;
                    match value {
                        SetValue::String(s) => write!(f, "'{}'", s.replace('\'', "''"))?,
                        SetValue::Ident(s) | SetValue::Number(s) => f.write_str(s)?,
                        SetValue::Default => f.write_str("DEFAULT")?,
                    }
                }
                Ok(())
            }
            Self::ResetParameter(None) => f.write_str("RESET ALL"),
            Self::ResetParameter(Some(name)) => write!(f, "RESET {name}"),
            Self::CreateFunction(s) => s.fmt(f),
            Self::CreateTrigger(s) => s.fmt(f),
            Self::DropTrigger {
                name,
                table,
                if_exists,
            } => {
                f.write_str("DROP TRIGGER ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                write!(f, "{} ON {}", quote_ident(name), quote_ident(table))
            }
            Self::DropFunction { name, if_exists } => {
                f.write_str("DROP FUNCTION ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                write!(f, "{}", quote_ident(name))
            }
            Self::CreateSequence(s) => s.fmt(f),
            Self::AlterSequence(s) => s.fmt(f),
            Self::DropSequence { names, if_exists } => {
                f.write_str("DROP SEQUENCE ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", quote_ident(n))?;
                }
                Ok(())
            }
            Self::CreateView(v) => v.fmt(f),
            Self::DropView { names, if_exists } => {
                f.write_str("DROP VIEW ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", quote_ident(n))?;
                }
                Ok(())
            }
            Self::CreateMaterializedView(v) => v.fmt(f),
            Self::RefreshMaterializedView { name, with_data } => {
                write!(f, "REFRESH MATERIALIZED VIEW {}", quote_ident(name))?;
                if !*with_data {
                    f.write_str(" WITH NO DATA")?;
                }
                Ok(())
            }
            Self::DropMaterializedView { names, if_exists } => {
                f.write_str("DROP MATERIALIZED VIEW ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", quote_ident(n))?;
                }
                Ok(())
            }
            Self::CreateType(t) => t.fmt(f),
            Self::DropType { names, if_exists } => {
                f.write_str("DROP TYPE ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", quote_ident(n))?;
                }
                Ok(())
            }
            Self::CreateDomain(d) => d.fmt(f),
            Self::DropDomain { names, if_exists } => {
                f.write_str("DROP DOMAIN ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", quote_ident(n))?;
                }
                Ok(())
            }
            Self::CreateSchema { name, if_not_exists } => {
                f.write_str("CREATE SCHEMA ")?;
                if *if_not_exists {
                    f.write_str("IF NOT EXISTS ")?;
                }
                write!(f, "{}", quote_ident(name))
            }
            Self::DropSchema { names, if_exists } => {
                f.write_str("DROP SCHEMA ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", quote_ident(n))?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for CreateDomainStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE DOMAIN {} AS {}", quote_ident(&self.name), self.base_type)?;
        if let Some(d) = &self.default {
            write!(f, " DEFAULT {d}")?;
        }
        if self.not_null {
            f.write_str(" NOT NULL")?;
        }
        for c in &self.checks {
            write!(f, " CHECK ({c})")?;
        }
        Ok(())
    }
}

impl fmt::Display for CreateTypeStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE TYPE {} AS ", quote_ident(&self.name))?;
        match &self.kind {
            TypeKind::Enum { labels } => {
                f.write_str("ENUM (")?;
                for (i, l) in labels.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "'{}'", l.replace('\'', "''"))?;
                }
                f.write_str(")")
            }
        }
    }
}

impl fmt::Display for CreateMaterializedViewStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE MATERIALIZED VIEW ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(f, "{}", quote_ident(&self.name))?;
        if !self.columns.is_empty() {
            f.write_str(" (")?;
            for (i, c) in self.columns.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", quote_ident(c))?;
            }
            f.write_str(")")?;
        }
        write!(f, " AS {}", self.body)?;
        if !self.with_data {
            f.write_str(" WITH NO DATA")?;
        }
        Ok(())
    }
}

impl fmt::Display for CreateViewStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE ")?;
        if self.or_replace {
            f.write_str("OR REPLACE ")?;
        }
        if self.temporary {
            f.write_str("TEMPORARY ")?;
        }
        f.write_str("VIEW ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(f, "{}", quote_ident(&self.name))?;
        if !self.columns.is_empty() {
            f.write_str(" (")?;
            for (i, c) in self.columns.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", quote_ident(c))?;
            }
            f.write_str(")")?;
        }
        write!(f, " AS {}", self.body)
    }
}

impl fmt::Display for CreateSequenceStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE ")?;
        if self.temporary {
            f.write_str("TEMPORARY ")?;
        }
        f.write_str("SEQUENCE ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(f, "{}", quote_ident(&self.name))?;
        if let Some(dt) = self.data_type {
            write!(f, " AS {dt}")?;
        }
        write_sequence_options(f, &self.options)
    }
}

impl fmt::Display for AlterSequenceStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ALTER SEQUENCE ")?;
        if self.if_exists {
            f.write_str("IF EXISTS ")?;
        }
        write!(f, "{}", quote_ident(&self.name))?;
        write_sequence_options(f, &self.options)
    }
}

impl fmt::Display for SequenceDataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SmallInt => "smallint",
            Self::Int => "integer",
            Self::BigInt => "bigint",
        })
    }
}

fn write_sequence_options(f: &mut fmt::Formatter<'_>, o: &SequenceOptions) -> fmt::Result {
    if let Some(n) = o.increment {
        write!(f, " INCREMENT BY {n}")?;
    }
    match o.min_value {
        Some(SeqBound::Value(n)) => write!(f, " MINVALUE {n}")?,
        Some(SeqBound::NoBound) => f.write_str(" NO MINVALUE")?,
        None => {}
    }
    match o.max_value {
        Some(SeqBound::Value(n)) => write!(f, " MAXVALUE {n}")?,
        Some(SeqBound::NoBound) => f.write_str(" NO MAXVALUE")?,
        None => {}
    }
    if let Some(n) = o.start {
        write!(f, " START WITH {n}")?;
    }
    match o.restart {
        Some(Some(n)) => write!(f, " RESTART WITH {n}")?,
        Some(None) => f.write_str(" RESTART")?,
        None => {}
    }
    if let Some(n) = o.cache {
        write!(f, " CACHE {n}")?;
    }
    match o.cycle {
        Some(true) => f.write_str(" CYCLE")?,
        Some(false) => f.write_str(" NO CYCLE")?,
        None => {}
    }
    if let Some(ob) = &o.owned_by {
        match ob {
            SequenceOwnedBy::None => f.write_str(" OWNED BY NONE")?,
            SequenceOwnedBy::Column { table, column } => {
                write!(f, " OWNED BY {}.{}", quote_ident(table), quote_ident(column))?;
            }
        }
    }
    Ok(())
}

impl fmt::Display for CreateFunctionStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE ")?;
        if self.or_replace {
            f.write_str("OR REPLACE ")?;
        }
        write!(f, "FUNCTION {}(", quote_ident(&self.name))?;
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            match arg.mode {
                FunctionArgMode::In => {}
                FunctionArgMode::Out => f.write_str("OUT ")?,
                FunctionArgMode::InOut => f.write_str("INOUT ")?,
            }
            if let Some(name) = &arg.name {
                write!(f, "{} ", quote_ident(name))?;
            }
            match &arg.ty {
                FunctionArgType::Typed(t) => write!(f, "{t}")?,
                FunctionArgType::Raw(s) => f.write_str(s)?,
            }
        }
        f.write_str(") RETURNS ")?;
        match &self.returns {
            FunctionReturn::Trigger => f.write_str("TRIGGER")?,
            FunctionReturn::Void => f.write_str("VOID")?,
            FunctionReturn::Type(t) => write!(f, "{t}")?,
            FunctionReturn::Other(s) => f.write_str(s)?,
        }
        write!(f, " LANGUAGE {} AS $$", self.language)?;
        match &self.body {
            FunctionBody::PlPgSql(b) => write!(f, "\n{b}\n")?,
            FunctionBody::Raw(s) => f.write_str(s)?,
        }
        f.write_str("$$")
    }
}

impl fmt::Display for PlPgSqlBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.declarations.is_empty() {
            f.write_str("DECLARE\n")?;
            for d in &self.declarations {
                write!(f, "  {} ", quote_ident(&d.name))?;
                match &d.ty {
                    FunctionArgType::Typed(t) => write!(f, "{t}")?,
                    FunctionArgType::Raw(s) => f.write_str(s)?,
                }
                if let Some(e) = &d.default {
                    write!(f, " := {e}")?;
                }
                f.write_str(";\n")?;
            }
        }
        f.write_str("BEGIN\n")?;
        for stmt in &self.statements {
            writeln!(f, "  {stmt};")?;
        }
        f.write_str("END")
    }
}

impl fmt::Display for PlPgSqlStmt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Assign { target, value } => write!(f, "{target} := {value}"),
            Self::SelectInto { var, body } => write!(f, "{body} INTO {var}"),
            Self::Return(t) => match t {
                ReturnTarget::New => f.write_str("RETURN NEW"),
                ReturnTarget::Old => f.write_str("RETURN OLD"),
                ReturnTarget::Null => f.write_str("RETURN NULL"),
                ReturnTarget::Expr(e) => write!(f, "RETURN {e}"),
            },
            Self::If {
                branches,
                else_branch,
            } => {
                for (i, (cond, body)) in branches.iter().enumerate() {
                    if i == 0 {
                        write!(f, "IF {cond} THEN ")?;
                    } else {
                        write!(f, " ELSIF {cond} THEN ")?;
                    }
                    for (j, s) in body.iter().enumerate() {
                        if j > 0 {
                            f.write_str("; ")?;
                        }
                        write!(f, "{s}")?;
                    }
                }
                if !else_branch.is_empty() {
                    f.write_str(" ELSE ")?;
                    for (j, s) in else_branch.iter().enumerate() {
                        if j > 0 {
                            f.write_str("; ")?;
                        }
                        write!(f, "{s}")?;
                    }
                }
                f.write_str(" END IF")
            }
            Self::Raise {
                level,
                message,
                args,
            } => {
                let lvl = match level {
                    RaiseLevel::Notice => "NOTICE",
                    RaiseLevel::Warning => "WARNING",
                    RaiseLevel::Info => "INFO",
                    RaiseLevel::Log => "LOG",
                    RaiseLevel::Debug => "DEBUG",
                    RaiseLevel::Exception => "EXCEPTION",
                };
                write!(f, "RAISE {lvl} '{}'", message.replace('\'', "''"))?;
                for a in args {
                    write!(f, ", {a}")?;
                }
                Ok(())
            }
            Self::EmbeddedSql(s) => write!(f, "{s}"),
        }
    }
}

impl fmt::Display for AssignTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewColumn(c) => write!(f, "NEW.{}", quote_ident(c)),
            Self::OldColumn(c) => write!(f, "OLD.{}", quote_ident(c)),
            Self::Local(n) => f.write_str(n),
        }
    }
}

impl fmt::Display for CreateTriggerStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE ")?;
        if self.or_replace {
            f.write_str("OR REPLACE ")?;
        }
        write!(f, "TRIGGER {} ", quote_ident(&self.name))?;
        match self.timing {
            TriggerTiming::Before => f.write_str("BEFORE")?,
            TriggerTiming::After => f.write_str("AFTER")?,
            TriggerTiming::InsteadOf => f.write_str("INSTEAD OF")?,
        }
        for (i, e) in self.events.iter().enumerate() {
            if i == 0 {
                f.write_str(" ")?;
            } else {
                f.write_str(" OR ")?;
            }
            match e {
                TriggerEvent::Insert => f.write_str("INSERT")?,
                TriggerEvent::Update => {
                    f.write_str("UPDATE")?;
                    if !self.update_columns.is_empty() {
                        f.write_str(" OF ")?;
                        for (j, col) in self.update_columns.iter().enumerate() {
                            if j > 0 {
                                f.write_str(", ")?;
                            }
                            f.write_str(&quote_ident(col))?;
                        }
                    }
                }
                TriggerEvent::Delete => f.write_str("DELETE")?,
                TriggerEvent::Truncate => f.write_str("TRUNCATE")?,
            }
        }
        write!(f, " ON {} FOR EACH ", quote_ident(&self.table))?;
        match self.for_each {
            TriggerForEach::Row => f.write_str("ROW")?,
            TriggerForEach::Statement => f.write_str("STATEMENT")?,
        }
        write!(f, " EXECUTE FUNCTION {}()", quote_ident(&self.function))
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
            // v7.15.0 — preserve operator class on round-trip
            // (`(col opclass)`) so WAL replay reconstructs the
            // engine-routing intent (e.g. `gin_trgm_ops` →
            // trigram-GIN build path).
            if let Some(op) = &self.opclass {
                write!(f, "({} {})", quote_ident(&self.column), op)?;
            } else {
                write!(f, "({})", quote_ident(&self.column))?;
            }
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
        // v7.13.0 — render table-level constraints (PRIMARY KEY /
        // UNIQUE / CHECK) so WAL replay reconstructs them. Inline
        // column-level UNIQUE / CHECK get lifted to this list at
        // parse time, so emitting only here avoids double-counting.
        for tc in &self.table_constraints {
            f.write_str(", ")?;
            write!(f, "{tc}")?;
        }
        f.write_str(")")
    }
}

fn fmt_alter_target(f: &mut fmt::Formatter<'_>, t: &AlterTableTarget) -> fmt::Result {
    match t {
        AlterTableTarget::SetHotTierBytes(n) => {
            write!(f, "SET hot_tier_bytes = {n}")
        }
        AlterTableTarget::AddForeignKey(fk) => write!(f, "ADD {fk}"),
        AlterTableTarget::DropForeignKey { name, if_exists } => {
            f.write_str("DROP CONSTRAINT ")?;
            if *if_exists {
                f.write_str("IF EXISTS ")?;
            }
            write!(f, "{}", quote_ident(name))
        }
        AlterTableTarget::AddColumn {
            column,
            if_not_exists,
        } => {
            f.write_str("ADD COLUMN ")?;
            if *if_not_exists {
                f.write_str("IF NOT EXISTS ")?;
            }
            write!(f, "{} {}", quote_ident(&column.name), column.ty)?;
            if !column.nullable {
                f.write_str(" NOT NULL")?;
            }
            if let Some(d) = &column.default {
                write!(f, " DEFAULT {d}")?;
            }
            if column.auto_increment {
                f.write_str(" AUTO_INCREMENT")?;
            }
            if column.is_primary_key {
                f.write_str(" PRIMARY KEY")?;
            }
            Ok(())
        }
        AlterTableTarget::AlterColumnType {
            column,
            new_type,
            using,
        } => {
            write!(f, "ALTER COLUMN {} TYPE {new_type}", quote_ident(column))?;
            if let Some(u) = using {
                write!(f, " USING {u}")?;
            }
            Ok(())
        }
        AlterTableTarget::DropColumn {
            column,
            if_exists,
            cascade,
        } => {
            f.write_str("DROP COLUMN ")?;
            if *if_exists {
                f.write_str("IF EXISTS ")?;
            }
            write!(f, "{}", quote_ident(column))?;
            if *cascade {
                f.write_str(" CASCADE")?;
            }
            Ok(())
        }
        AlterTableTarget::AddTableConstraint(tc) => {
            write!(f, "ADD {tc}")
        }
        AlterTableTarget::RenameColumn { old, new } => {
            write!(
                f,
                "RENAME COLUMN {} TO {}",
                quote_ident(old),
                quote_ident(new)
            )
        }
        AlterTableTarget::RenameTable { new } => {
            write!(f, "RENAME TO {}", quote_ident(new))
        }
        AlterTableTarget::SetTriggerEnabled { which, enabled } => {
            f.write_str(if *enabled {
                "ENABLE TRIGGER "
            } else {
                "DISABLE TRIGGER "
            })?;
            match which {
                TriggerSelector::All => f.write_str("ALL"),
                TriggerSelector::Named(n) => f.write_str(&quote_ident(n)),
            }
        }
    }
}

impl fmt::Display for TableConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrimaryKey { name, columns } => {
                if let Some(n) = name {
                    write!(f, "CONSTRAINT {} ", quote_ident(n))?;
                }
                f.write_str("PRIMARY KEY (")?;
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&quote_ident(c))?;
                }
                f.write_str(")")
            }
            Self::Unique {
                name,
                columns,
                nulls_not_distinct,
            } => {
                if let Some(n) = name {
                    write!(f, "CONSTRAINT {} ", quote_ident(n))?;
                }
                f.write_str("UNIQUE ")?;
                if *nulls_not_distinct {
                    f.write_str("NULLS NOT DISTINCT ")?;
                }
                f.write_str("(")?;
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&quote_ident(c))?;
                }
                f.write_str(")")
            }
            Self::Check { name, expr } => {
                if let Some(n) = name {
                    write!(f, "CONSTRAINT {} ", quote_ident(n))?;
                }
                write!(f, "CHECK ({expr})")
            }
            Self::Index { name, columns } => {
                f.write_str("KEY ")?;
                if let Some(n) = name {
                    write!(f, "{} ", quote_ident(n))?;
                }
                f.write_str("(")?;
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&quote_ident(c))?;
                }
                f.write_str(")")
            }
            Self::FulltextIndex { name, columns } => {
                // Mysqldump emits `FULLTEXT KEY name (cols)` —
                // Display rounds back to that shape so dump
                // replay reproduces the input verbatim.
                f.write_str("FULLTEXT KEY ")?;
                if let Some(n) = name {
                    write!(f, "{} ", quote_ident(n))?;
                }
                f.write_str("(")?;
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&quote_ident(c))?;
                }
                f.write_str(")")
            }
        }
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
        // v7.17.0 Phase 2.5 — render COLLATE for round-trippable
        // DDL. Only emits when non-default so the typical output
        // stays unchanged.
        match self.collation {
            Collation::Binary => {}
            Collation::CaseInsensitive => f.write_str(" COLLATE \"case_insensitive\"")?,
        }
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
        // v7.13.0 — INSERT…SELECT renders as `... SELECT …`,
        // skipping the VALUES list (mailrs round-5 G4).
        if let Some(sel) = &self.select_source {
            write!(f, " {sel}")?;
        } else {
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
            Self::Case {
                operand,
                branches,
                else_branch,
            } => {
                f.write_str("CASE")?;
                if let Some(op) = operand {
                    write!(f, " {op}")?;
                }
                for (w, t) in branches {
                    write!(f, " WHEN {w} THEN {t}")?;
                }
                if let Some(e) = else_branch {
                    write!(f, " ELSE {e}")?;
                }
                f.write_str(" END")
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
            Self::InetContainedBy => "<<",
            Self::InetContainedByEq => "<<=",
            Self::InetContains => ">>",
            Self::InetContainsEq => ">>=",
            Self::InetOverlap => "&&",
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
                    unnest_column_aliases: Vec::new(),
                    generate_series_args: None,
                    lateral_subquery: None,
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
            limit_with_ties: false,
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
