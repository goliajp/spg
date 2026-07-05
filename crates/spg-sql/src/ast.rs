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
    /// `COPY table [(cols)] TO STDOUT` — the engine renders the
    /// visible rows in COPY text format (tab-separated, `\N`
    /// nulls, backslash escapes) as a single-text-column result
    /// set; the wire layer streams CopyData from it.
    CopyTo {
        table: String,
        columns: Option<Vec<String>>,
    },
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
    /// v7.17.0 Phase 3.P0-58 — MySQL `SHOW DATABASES` /
    /// `SHOW SCHEMAS`. SPG is single-database; the executor
    /// returns the canonical MySQL set so the mysql / MariaDB
    /// client populates its database selector.
    ShowDatabases,
    /// v7.17.0 Phase 3.P0-59 — MySQL `SHOW CREATE TABLE <t>`
    /// returns a 2-column row `(Table, "Create Table")` carrying
    /// the synthesized DDL. mysqldump emits this for every
    /// table at scrape time.
    ShowCreateTable(String),
    /// v7.17.0 Phase 3.P0-60 — MySQL `SHOW INDEXES FROM <t>`
    /// (also `SHOW INDEX`, `SHOW KEYS`).
    ShowIndexes(String),
    /// v7.17.0 Phase 3.P0-61 — MySQL `SHOW STATUS`.
    ShowStatus,
    /// v7.17.0 Phase 3.P0-61 — MySQL `SHOW VARIABLES`.
    ShowVariables,
    /// v7.17.0 Phase 3.P0-62 — MySQL `SHOW PROCESSLIST`.
    ShowProcesslist,
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
    /// v7.37.17 (17.6 sibling) — `TRUNCATE [TABLE] [ONLY] <name>
    /// [, ...] [RESTART IDENTITY | CONTINUE IDENTITY] [CASCADE |
    /// RESTRICT]`. Clears every row from each named table. SPG's
    /// SEQUENCE identity is per-table; RESTART IDENTITY reinitializes
    /// the associated sequence to its starting value. CASCADE
    /// currently walks direct FK-referring tables and truncates
    /// them too (PG's semantics). The ONLY modifier (skip partitions)
    /// and RESTRICT (default) are accepted with no effect since
    /// SPG's declarative partitions are always truncated together.
    Truncate {
        tables: Vec<String>,
        restart_identity: bool,
        cascade: bool,
    },
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
    /// v7.38 轴 4 — `SET [SESSION] TRANSACTION ISOLATION LEVEL …`
    /// (plus optional READ ONLY / READ WRITE / DEFERRABLE clauses
    /// silently accepted). PG-standard surface for picking an
    /// isolation level. Engine tracks the value on
    /// `Engine::current_isolation_level()`; actual MVCC / SSI
    /// semantics implementation lands separately. PG itself maps
    /// READ UNCOMMITTED to READ COMMITTED; SPG mirrors that —
    /// effectively every level reads as READ COMMITTED in v7.37.8.
    SetTransaction {
        isolation: IsolationLevel,
    },
    /// v7.38 轴 4 — `SHOW <param>` returns a 1-column 1-row result
    /// with the parameter's current value as TEXT. Today the only
    /// recognised param is `transaction_isolation`; further
    /// surfaces (`search_path`, `application_name`, …) land as the
    /// session-parameter inventory grows.
    ShowParameter(String),
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
    /// v7.37 D.55 — `ALTER TYPE name ADD VALUE [IF NOT EXISTS] 'label'
    /// [{BEFORE | AFTER} 'existing']`. Extends an enum's label list so
    /// enum evolution stops being a silent no-op. `position` is
    /// `Some((is_before, anchor))`.
    AlterTypeAddValue {
        type_name: String,
        label: String,
        if_not_exists: bool,
        position: Option<(bool, String)>,
    },
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
///
/// v7.37.x (ζ-B Phase 1 composite accept) — added Composite for
/// `CREATE TYPE name AS (field_name field_type, …)`. Phase 1
/// stores the field list in the catalog so PG dumps that emit
/// `CREATE TYPE … AS (…)` don't error out; using a composite type
/// as a column type lands in Phase 2 (Value::Composite encoding +
/// ROW() literal + field-access syntax).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    /// `AS ENUM ('a', 'b', …)`. Order is preserved (PG enum
    /// labels are ordered).
    Enum { labels: Vec<String> },
    /// `AS (field_name field_type, …)`. Order matters; PG
    /// composite literals are positional.
    Composite {
        fields: Vec<(String, ColumnTypeName)>,
    },
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

/// v7.38 轴 4 — PG-standard isolation levels. SPG accepts all four
/// at parse time and tracks the selected value on the engine. The
/// actual semantic differentiation (REPEATABLE READ snapshot,
/// SERIALIZABLE SSI) lands in the v7.38 isolation framework train;
/// today every level reads as effective READ COMMITTED (which is
/// also how PG treats READ UNCOMMITTED — it silently upgrades to
/// READ COMMITTED). Default = `ReadCommitted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationLevel {
    ReadUncommitted,
    #[default]
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl IsolationLevel {
    /// Canonical PG-style display name, as `SHOW transaction_isolation`
    /// would return it (`"read committed"`, `"repeatable read"`,
    /// `"serializable"`). `READ UNCOMMITTED` maps to `"read committed"`
    /// per the documented PG behaviour.
    pub fn as_pg_str(self) -> &'static str {
        match self {
            Self::ReadUncommitted | Self::ReadCommitted => "read committed",
            Self::RepeatableRead => "repeatable read",
            Self::Serializable => "serializable",
        }
    }
}

impl core::fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_pg_str())
    }
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
#[allow(clippy::large_enum_variant)]
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
    /// v7.22 (round-13 T2) — mark a column auto-incrementing.
    /// pg_dump splits SERIAL/IDENTITY columns into a plain integer
    /// column plus either `ALTER COLUMN c SET DEFAULT nextval(…)`
    /// (serial) or `ALTER COLUMN c ADD GENERATED … AS IDENTITY (…)`
    /// (identity); both lower to this. SPG's auto-increment is
    /// max+1-scan based, so the dump's `setval(…)` calls stay
    /// no-ops without losing the sequence position.
    SetColumnAutoIncrement {
        column: String,
        /// The implicit sequence pg_dump names for an identity
        /// column (`ADD GENERATED … ( SEQUENCE NAME s … )`) or the
        /// nextval target for a serial default. The engine creates
        /// it if absent so the dump's later `setval(s, …)` lands.
        seq_name: Option<String>,
    },
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
    /// v7.37.16 (16.3) — `ALTER TABLE parent ATTACH PARTITION child
    /// <bounds>`. Promotes an existing table `child` to a partition
    /// of `parent` using PG-style `FOR VALUES …` / `DEFAULT` bounds.
    /// Engine validates that `child`'s columns are layout-compatible
    /// with `parent` and that every row in `child` satisfies the
    /// bound before installing the role.
    AttachPartition {
        child: String,
        bounds: PartitionOfBoundsAst,
    },
    /// v7.37.16 (16.4 + 16.5) — `ALTER TABLE parent DETACH PARTITION
    /// child [CONCURRENTLY] [FINALIZE]`. Demotes a partition back
    /// to a standalone table (clears `partition_role`) and removes
    /// it from the parent's child set. v7.37.16.5: `CONCURRENTLY`
    /// is parser-accepted; engine performs the same atomic detach
    /// (single-engine, no replication lag — the PG semantics that
    /// require the two-phase split don't apply).
    DetachPartition {
        child: String,
        concurrently: bool,
        finalize: bool,
    },
    /// v7.37.18 (18.1) — `ALTER TABLE … ALTER COLUMN col SET DEFAULT
    /// <expr>`. Engine re-parses + freezes the literal at this point,
    /// matching CREATE TABLE-side default semantics. Volatile shapes
    /// (`now()` / `nextval`) take the runtime-default path.
    AlterColumnSetDefault {
        column: String,
        default_expr: Expr,
    },
    /// v7.37.18 (18.1) — `ALTER TABLE … ALTER COLUMN col DROP DEFAULT`.
    AlterColumnDropDefault { column: String },
    /// v7.37.18 (18.2) — `ALTER TABLE … ALTER COLUMN col SET NOT NULL`.
    /// Engine validates that no existing row has NULL in that column
    /// before flipping the flag (PG semantics — partial NOT NULL
    /// would surface inconsistently).
    AlterColumnSetNotNull { column: String },
    /// v7.37.18 (18.2) — `ALTER TABLE … ALTER COLUMN col DROP NOT NULL`.
    AlterColumnDropNotNull { column: String },
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
    /// v7.37.7 — `EXPLAIN (COSTS OFF) <SELECT>` strips wall-clock
    /// `elapsed=…us` annotations from the Total line (and any
    /// future cost-bearing lines). PG-standard option used by
    /// regression suites and diff-friendly EXPLAIN output. When
    /// `true`, takes precedence over the per-session
    /// `SPG_TEST_EXPLAIN_NO_COSTS` GUC.
    pub costs_off: bool,
    /// v7.37.22 (22.7) — `EXPLAIN (BUFFERS) <SELECT>`. PG-standard
    /// option that surfaces hot/cold/shared block counters. SPG's
    /// hot-tier scan path counts examined rows; the BUFFERS option
    /// makes that an explicit per-operator annotation.
    pub buffers: bool,
    /// v7.37.22 (22.7) — `EXPLAIN (TIMING [ON|OFF]) <SELECT>`. PG
    /// uses this to disable per-operator timing while still
    /// emitting actual-row counts (cheaper than ANALYZE). Default
    /// when EXPLAIN ANALYZE is set: TIMING ON. `false` strips the
    /// timing portion of the Total line. Decoupled from `costs_off`:
    /// PG's COSTS OFF strips estimated cost; TIMING OFF strips
    /// measured wall-clock.
    pub timing_off: bool,
    /// v7.37.22 (22.7) — `EXPLAIN (SETTINGS) <SELECT>`. PG appends
    /// modified GUC values to the plan output. SPG emits the
    /// session params that diverge from default after the main
    /// plan body.
    pub settings: bool,
    /// v7.37.22 (22.7) — `EXPLAIN (WAL) <SELECT>`. PG counts WAL
    /// bytes / records / FPI emitted by the query. SPG's
    /// write-side queries (INSERT/UPDATE/DELETE wrapped in EXPLAIN
    /// ANALYZE) report against the engine WAL counter delta.
    pub wal: bool,
    /// v7.37.23 (23.5) — `EXPLAIN (FORMAT text|json|xml|yaml)`.
    /// PG's standard format selector. Default is text. JSON / XML
    /// / YAML emit a single-row TEXT result whose body wraps the
    /// existing line-per-operator text in the chosen container —
    /// PG-compatible just enough for dashboards that parse those
    /// container shapes (pgAdmin's JSON path picker, etc.).
    pub format: ExplainFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExplainFormat {
    #[default]
    Text,
    Json,
    Xml,
    Yaml,
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
    /// v7.37.20 (20.10) — `EXCEPTION WHEN <cond> [OR <cond>...] THEN
    /// <body>` handlers appended to the block. Empty when no
    /// EXCEPTION clause is present. When a body statement raises
    /// (via RAISE EXCEPTION, ASSERT falsy, or a runtime error),
    /// handlers are tried in order; the first matching condition
    /// runs its body and the block terminates cleanly. `OTHERS`
    /// matches any exception. Unhandled exceptions propagate.
    pub exception_handlers: Vec<ExceptionHandler>,
}

/// v7.37.20 (20.10) — one `WHEN <cond> [OR <cond>...] THEN <body>`
/// arm inside an EXCEPTION block.
#[derive(Debug, Clone, PartialEq)]
pub struct ExceptionHandler {
    /// Condition names (`OTHERS`, `unique_violation`, etc.). Multiple
    /// conditions joined by `OR` share one handler body.
    pub conditions: Vec<String>,
    /// Statements to run when a matching exception is caught.
    pub body: Vec<PlPgSqlStmt>,
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
    /// v7.37.20 (20.14) — `ASSERT <condition> [, <message>];`. If
    /// the condition evaluates falsy the trigger / DO block aborts
    /// with the message (defaulting to a generic shape when none
    /// is provided). Same propagation shape as `RAISE EXCEPTION`
    /// — the error reaches the caller's query path. PG's behaviour
    /// is identical except for a `plpgsql.check_asserts` GUC that
    /// can disable the check globally; SPG always evaluates.
    Assert {
        condition: Expr,
        message: Option<Expr>,
    },
    /// v7.37.20 (20.3) — `WHILE <condition> LOOP <body> END LOOP;`.
    /// Iterate the body while condition evaluates truthy. Iteration
    /// count is bounded by `WHILE_LOOP_BUDGET` to prevent runaway
    /// loops; the executor errors out when reached. EXIT / CONTINUE
    /// inside the body queue with 20.2.
    While {
        condition: Expr,
        body: Vec<PlPgSqlStmt>,
    },
    /// v7.37.20 (20.4) — `FOR <var> IN [REVERSE] <start>..<end> LOOP
    /// <body> END LOOP;`. Integer iteration; `var` is BigInt-valued;
    /// bounds inclusive on both sides. REVERSE walks backward.
    /// Iteration budget guards runaway.
    ForRange {
        var: String,
        start: Expr,
        end: Expr,
        reverse: bool,
        body: Vec<PlPgSqlStmt>,
    },
    /// v7.37.20 (20.2) — bare `LOOP <body> END LOOP;`. Runs the body
    /// repeatedly; only `EXIT [WHEN <cond>]` breaks out. Iteration
    /// budget guards runaway.
    Loop { body: Vec<PlPgSqlStmt> },
    /// v7.37.20 (20.2) — `EXIT [WHEN <condition>];` inside a loop.
    /// Unconditional (no WHEN) or conditional (only breaks when
    /// condition is truthy). Bubbles up as BodyOutcome::Break which
    /// the enclosing loop catches. Outside a loop it's a no-op.
    Exit { when: Option<Expr> },
    /// v7.37.20 (20.2) — `CONTINUE [WHEN <condition>];` inside a
    /// loop. Same shape as EXIT but bubbles up as BodyOutcome::Continue
    /// which the enclosing loop catches, skipping the remainder of
    /// the body and jumping to the next iteration.
    Continue { when: Option<Expr> },
    /// v7.37.20 (20.13) — `EXECUTE <string_expr>;` runs a runtime-
    /// computed SQL statement. The expression is evaluated to a
    /// text value, the resulting string is parsed and dispatched
    /// through the engine like an EmbeddedSql. USING <param_list>
    /// for placeholder binding queues with v7.40 PL/pgSQL epic.
    ExecuteDynamic { sql: Expr },
    /// v7.37.20 (20.5) — `FOR <var> IN <select_body> LOOP <body>
    /// END LOOP;`. Runs the SELECT once, iterates the resulting
    /// rows, binds the first column of each row to `var` as a
    /// scalar Value, then runs the body per iteration. EXIT /
    /// CONTINUE / ASSERT / RAISE etc. propagate through the
    /// enclosing loop's BodyOutcome discipline the same way
    /// FOR range and WHILE do. Full record-binding (var as
    /// composite carrying all columns) queues with v7.40 record
    /// type infrastructure.
    ForQuery {
        var: String,
        query: Box<SelectStatement>,
        body: Vec<PlPgSqlStmt>,
    },
    /// v7.37.20 (20.6) — `FOR <var> IN EXECUTE <string_expr> LOOP
    /// <body> END LOOP;`. Same shape as ForQuery but the SELECT is
    /// computed at runtime from a text expression, parsed on the
    /// fly, then iterated. Enables dynamic queries where the
    /// projection / FROM / WHERE clauses depend on runtime values.
    ForExecute {
        var: String,
        sql_expr: Expr,
        body: Vec<PlPgSqlStmt>,
    },
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
    /// v7.37.6-B(sentori Epic 2 P0)— `PARTITION BY <strategy>
    /// (key_col)` declarative partition-parent suffix. `Some` ⇒
    /// the engine creates a parent table whose own rows stay
    /// empty and routes INSERT/SELECT through children. Mutually
    /// exclusive with `partition_of` (parser enforces).
    pub partition_by: Option<PartitionBySpec>,
    /// v7.37.6-B — `PARTITION OF <parent> { FOR VALUES FROM (a)
    /// TO (b) | DEFAULT }` child-table declaration. `Some` ⇒
    /// the table inherits its column list from `parent` (the
    /// parser rejects an explicit column list when this is set);
    /// engine routes child rows back to the parent at INSERT.
    pub partition_of: Option<PartitionOfSpec>,
}

/// v7.37.6-B — `PARTITION BY <kind> (key_columns…)` parent suffix.
/// v7.37.6-B only RANGE is recognised; the enum keeps space for
/// future LIST / HASH without breaking the public AST shape.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionBySpec {
    pub kind: PartitionKindAst,
    /// One or more ident references into the parent's column list.
    /// v7.37.6-B contracts a single TIMESTAMPTZ key; multi-key
    /// RANGE is a phase-2 extension. Parser allows ≥1 to keep the
    /// shape PG-compatible.
    pub key_columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKindAst {
    Range,
    /// v7.37.16 (16.1) — `PARTITION BY LIST (key)`. Child uses
    /// `FOR VALUES IN (lit, lit, …)`.
    List,
    /// v7.37.16 (16.2) — `PARTITION BY HASH (key)`. Child uses
    /// `FOR VALUES WITH (MODULUS m, REMAINDER r)`.
    Hash,
}

/// v7.37.6-B — `PARTITION OF <parent> <bounds>` child suffix.
/// Bounds is either a half-open range (`FOR VALUES FROM (a) TO (b)`)
/// or the catch-all `DEFAULT` partition.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionOfSpec {
    pub parent_name: String,
    pub bounds: PartitionOfBoundsAst,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PartitionOfBoundsAst {
    /// `FOR VALUES FROM (lower) TO (upper)`. `Expr` is ~144 bytes
    /// (lits include vector bodies), so we box both bounds to keep
    /// the variant size in line with `Default` for clippy and to
    /// minimise per-statement footprint when the partition shape
    /// isn't in use.
    Range {
        lower: Box<Expr>,
        upper: Box<Expr>,
    },
    /// v7.37.16 (16.1) — `FOR VALUES IN (lit [, lit, …])`. Each
    /// expr resolves to a typed literal at child-create time.
    List {
        values: Vec<Expr>,
    },
    /// v7.37.16 (16.2) — `FOR VALUES WITH (MODULUS m, REMAINDER r)`.
    /// PG enforces `0 ≤ r < m`; m must be positive.
    Hash {
        modulus: u32,
        remainder: u32,
    },
    Default,
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
    /// v7.37.7(sentori Epic 3 P1)— `GENERATED ALWAYS AS (<expr>)
    /// STORED` computed-column source. When `Some`, the engine
    /// stores the Display-form of the parsed expression on
    /// `ColumnSchema.generated_stored_expr` at CREATE TABLE time
    /// and re-evaluates the expression against every INSERT /
    /// UPDATE candidate row, overwriting whatever the caller
    /// supplied for this column. Boxed to keep `ColumnDef` from
    /// blowing past the `large_enum_variant` clippy ceiling
    /// (`Expr` widens with vector literals).
    pub generated_stored_expr: Option<Box<Expr>>,
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

#[allow(clippy::derivable_impls)]
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
    /// v7.37.5 β-P2 — `INTERVAL` as a column type. Storage is the
    /// three-field {months, days, micros} struct (PG-byte-equal),
    /// catalog tag 34, FILE_VERSION 48+. Wire OID 1186. Prior to
    /// β-P2 `INTERVAL` was runtime-only — literal in expression
    /// position but rejected at CREATE TABLE.
    Interval,
    /// v7.37.5 β-P4 — `INTERVAL[]` — single-dimension array of
    /// INTERVAL. Wire OID 1187 (`_interval`). Catalog tag 35.
    /// PG external form quotes each non-NULL element because
    /// interval text contains spaces / colons
    /// (`{"1 day","24:00:00",NULL}`).
    IntervalArray,
    /// v7.37.5 γ — full PG array-of-scalar family. Each variant
    /// mirrors a scalar `ColumnTypeName` that already existed.
    BoolArray,
    SmallIntArray,
    FloatArray,
    NumericArray,
    DateArray,
    TimestampArray,
    TimestamptzArray,
    UuidArray,
    JsonArray,
    JsonbArray,
    BytesArray,
    VarcharArray,
    CharArray,
    /// v7.37.5 δ — PG 14+ multirange types. Same wrapper pattern
    /// as `Range(RangeKindAst)` — one column type variant covers
    /// all six builtin multiranges, kind pins the element type.
    /// Wire OIDs in pgwire.
    Multirange(RangeKindAst),
    /// v7.37.5 ε — PG geometry scalar family. Each maps one-to-
    /// one to a PG type: point/lseg/path/box/polygon/line/circle.
    /// Wire OIDs in pgwire.
    Point,
    Lseg,
    Path,
    PgBox,
    Polygon,
    Line,
    Circle,
    /// v7.37.5 ζ-A — PG network / bit / xml / "char" / money[].
    Inet,
    Cidr,
    Macaddr,
    Macaddr8,
    Bit,
    BitVarying,
    Xml,
    Char1,
    MoneyArray,
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
            Self::Interval => f.write_str("INTERVAL"),
            Self::IntervalArray => f.write_str("INTERVAL[]"),
            Self::BoolArray => f.write_str("BOOL[]"),
            Self::SmallIntArray => f.write_str("SMALLINT[]"),
            Self::FloatArray => f.write_str("FLOAT[]"),
            Self::NumericArray => f.write_str("NUMERIC[]"),
            Self::DateArray => f.write_str("DATE[]"),
            Self::TimestampArray => f.write_str("TIMESTAMP[]"),
            Self::TimestamptzArray => f.write_str("TIMESTAMPTZ[]"),
            Self::UuidArray => f.write_str("UUID[]"),
            Self::JsonArray => f.write_str("JSON[]"),
            Self::JsonbArray => f.write_str("JSONB[]"),
            Self::BytesArray => f.write_str("BYTEA[]"),
            Self::VarcharArray => f.write_str("VARCHAR[]"),
            Self::CharArray => f.write_str("CHAR[]"),
            Self::Multirange(k) => f.write_str(match k {
                RangeKindAst::Int4 => "INT4MULTIRANGE",
                RangeKindAst::Int8 => "INT8MULTIRANGE",
                RangeKindAst::Num => "NUMMULTIRANGE",
                RangeKindAst::Ts => "TSMULTIRANGE",
                RangeKindAst::TsTz => "TSTZMULTIRANGE",
                RangeKindAst::Date => "DATEMULTIRANGE",
            }),
            Self::Point => f.write_str("POINT"),
            Self::Lseg => f.write_str("LSEG"),
            Self::Path => f.write_str("PATH"),
            Self::PgBox => f.write_str("BOX"),
            Self::Polygon => f.write_str("POLYGON"),
            Self::Line => f.write_str("LINE"),
            Self::Circle => f.write_str("CIRCLE"),
            Self::Inet => f.write_str("INET"),
            Self::Cidr => f.write_str("CIDR"),
            Self::Macaddr => f.write_str("MACADDR"),
            Self::Macaddr8 => f.write_str("MACADDR8"),
            Self::Bit => f.write_str("BIT"),
            Self::BitVarying => f.write_str("VARBIT"),
            Self::Xml => f.write_str("XML"),
            Self::Char1 => f.write_str("\"char\""),
            Self::MoneyArray => f.write_str("MONEY[]"),
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
    /// v7.37.43-T4.4 — leading `WITH cte AS (…)` clauses on a top-
    /// level UPDATE. Empty for a plain UPDATE.
    pub ctes: Vec<Cte>,
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
    /// v7.37.43-T4.4 — leading `WITH cte AS (…)` clauses on a top-
    /// level DELETE. Empty for a plain DELETE.
    pub ctes: Vec<Cte>,
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
    /// v7.37 D.44 — `USING (SELECT …) alias` subquery source. When present,
    /// the engine materialises this SELECT for the source rows and `source`
    /// is empty; the alias (required by PG for a subquery source) is in
    /// `source_alias`. `None` = plain `USING <table>` (source names a table).
    pub source_select: Option<Box<SelectStatement>>,
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
    /// v7.37.43-T4.4 — leading `WITH cte AS (…)` clauses on a top-
    /// level INSERT (writable CTE outer body). Empty for a plain
    /// INSERT. PG semantics: each CTE materialises before the
    /// outer INSERT runs, sharing the same transaction.
    pub ctes: Vec<Cte>,
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
    /// v7.37.17 (17.6 siblings) — `ON CONFLICT ON CONSTRAINT
    /// <name>`: the pg_dump conflict-target form. The engine
    /// resolves the name to the constraint's columns.
    pub constraint_name: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SelectStatement {
    /// v4.11: `WITH name AS (SELECT ...) [, ...]` common-table
    /// expressions, materialised once at query start before the
    /// body SELECT runs. Empty for a regular SELECT. Non-recursive
    /// only — no `WITH RECURSIVE` for v4.x.
    pub ctes: Vec<Cte>,
    pub distinct: bool,
    /// v7.37.17 (17.6 siblings) — `SELECT DISTINCT ON (exprs)`:
    /// keep the first row (per ORDER BY) of each group the
    /// expressions define. Empty = no DISTINCT ON.
    pub distinct_on: Vec<Expr>,
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
    /// v7.37.43-T4.4 — body is either a SELECT (read-only CTE, the
    /// classical case) or a data-modifying statement
    /// (INSERT / UPDATE / DELETE … RETURNING …) per PG writable
    /// CTE semantics. The modifying body's RETURNING projection
    /// becomes the materialised CTE table the outer query can
    /// reference; the modifying statement runs once before the
    /// outer query, within the same transaction.
    pub body: CteBody,
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

/// v7.37.43-T4.4 — CTE body. Read-only (Select) or data-modifying
/// (Insert / Update / Delete with optional RETURNING). The
/// data-modifying variants must carry a RETURNING projection for the
/// outer query to reference the CTE alias by; an empty RETURNING is
/// only valid if no outer reference materialises (rare — typically
/// caught at planning).
#[allow(clippy::large_enum_variant)] // CteBody::Select dominates; Boxing would touch every match site
#[derive(Debug, Clone, PartialEq)]
pub enum CteBody {
    Select(SelectStatement),
    Insert(Box<InsertStatement>),
    Update(Box<UpdateStatement>),
    Delete(Box<DeleteStatement>),
}

impl CteBody {
    /// Convenience accessor used by classical (read-only) CTE
    /// callsites that still expect a SELECT body. Returns None for
    /// data-modifying CTEs; callers must explicitly route those
    /// through `exec_with_ctes`'s modifying branch.
    #[must_use]
    pub fn as_select(&self) -> Option<&SelectStatement> {
        match self {
            Self::Select(s) => Some(s),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_select_mut(&mut self) -> Option<&mut SelectStatement> {
        match self {
            Self::Select(s) => Some(s),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_modifying(&self) -> bool {
        !matches!(self, Self::Select(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    /// `false` = ASC (default), `true` = DESC.
    pub desc: bool,
    /// v7.24 (mailrs round-16 A) — explicit `NULLS FIRST` /
    /// `NULLS LAST`. `None` = PG default (NULLS LAST for ASC,
    /// NULLS FIRST for DESC); the engine resolves the effective
    /// value via `nulls_first.unwrap_or(desc)`.
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnionKind {
    /// `UNION` — dedupes the combined set.
    Distinct,
    /// `UNION ALL` — concatenates without dedup.
    All,
    /// v7.37.17 (17.6 siblings) — `INTERSECT`: distinct rows
    /// present on both sides.
    Intersect,
    /// `INTERSECT ALL` — multiset intersection (min per-row count).
    IntersectAll,
    /// `EXCEPT` — distinct left rows absent from the right.
    Except,
    /// `EXCEPT ALL` — multiset subtraction.
    ExceptAll,
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
    /// `WITH ORDINALITY` on an unnest-channel SRF — when true, the
    /// row-stream gains a trailing BIGINT column counting rows
    /// from 1 in element order. PG names it `ordinality`; a second
    /// entry in the column-alias list renames it.
    pub with_ordinality: bool,
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
    /// v7.37.43-T4.5 — `jsonb_each_text(<expr>)` set-returning
    /// function as a FROM item. PG semantics: for each key/value
    /// pair in the JSONB object argument, emit one (key TEXT,
    /// value TEXT) row. When prefixed by `LATERAL` and joined via
    /// `CROSS JOIN LATERAL`, the argument may reference columns
    /// from a preceding FROM item, in which case the executor
    /// evaluates `<expr>` per outer row.
    /// Mutually exclusive with `unnest_expr` / `generate_series_args`
    /// / `lateral_subquery`. The optional `LATERAL` keyword does not
    /// require a separate flag — the executor evaluates per-row
    /// whenever the join sits in a JoinKind context.
    ///
    /// v7.37.17 (17.6 siblings) — the tuple's first slot carries the
    /// lowercase SRF name (`jsonb_each` / `jsonb_each_text` /
    /// `json_each` / `json_each_text`) so the executor picks the
    /// value-column rendering (JSON text vs unwrapped text).
    pub jsonb_each_text_arg: Option<(String, Box<Expr>)>,
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
    /// v7.37.16 — `JOIN … USING (c1, c2, …)`. When `Some`, records the
    /// USING column list so the executor can perform PG's column-merge
    /// (the join columns collapse to a single unqualified output column,
    /// `t1.c` for INNER/LEFT, `t2.c` for RIGHT, `COALESCE(t1.c,t2.c)` for
    /// FULL, and appear first in `SELECT *`). The parser ALSO desugars
    /// USING into an equivalent `on` predicate so the join filter/count
    /// path works unchanged; `using_cols` drives only the output-shape
    /// rewrite. Empty/`None` for `ON` and CROSS joins.
    pub using_cols: Option<Vec<String>>,
    /// v7.37.16 — `NATURAL [INNER|LEFT|RIGHT|FULL] JOIN`. The common
    /// column names are not known until the table schemas are available
    /// (parse time is schema-less), so the parser only sets this flag and
    /// leaves `on`/`using_cols` empty; the engine resolves the common
    /// columns at execution time, synthesises the `on` predicate + the
    /// USING column-merge, and clears the flag. If there are no common
    /// columns PG treats it as a CROSS join.
    pub natural: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
    /// v7.37.16 — `RIGHT [OUTER] JOIN`: keep every right (peer) row,
    /// NULL-filling the left (drive) columns on unmatched right rows.
    /// The executor runs the LEFT algorithm's mirror: it tracks which
    /// peer rows matched and emits the unmatched ones with a NULL-left
    /// tuple after the probe loop. Output column order is unchanged
    /// (left-table cols then right-table cols).
    Right,
    /// v7.37.16 — `FULL [OUTER] JOIN`: keep every row from both sides
    /// (LEFT-unmatched → NULL right, RIGHT-unmatched → NULL left).
    FullOuter,
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
    /// v7.24 (mailrs round-16 A) — an aggregate call with an
    /// internal ordering: `array_agg(x ORDER BY y DESC NULLS LAST)`.
    /// Wraps the plain [`Expr::FunctionCall`] so every existing
    /// FunctionCall consumer stays untouched; only the aggregate
    /// executor (and the expression walkers) know the wrapper.
    /// Non-aggregate evaluation contexts reject it at eval time.
    AggregateOrdered {
        call: Box<Expr>,
        order_by: Vec<OrderBy>,
        /// v7.25 (round-17) — `COUNT(DISTINCT x)` /
        /// `string_agg(DISTINCT s, ',')`. The wrapper carries every
        /// aggregate modifier so plain FunctionCall stays untouched.
        distinct: bool,
        /// v7.32 (mailrs round-29) — `agg(args) FILTER (WHERE cond)`.
        /// Only the rows where `cond` is true contribute to this
        /// aggregate (SQL:2003 T612 / PG 9.4). Carried as a first-class
        /// modifier — NOT desugared to `agg(CASE WHEN cond THEN arg
        /// END)`, which is faithful for NULL-ignoring aggregates but
        /// WRONG for `array_agg` (it would collect a NULL per excluded
        /// row). The executor instead skips excluded rows before
        /// accumulation, which is correct for every aggregate.
        filter: Option<Box<Expr>>,
    },
    /// SQL `LIKE` predicate. `pattern` evaluates to text at runtime;
    /// wildcards are `%` (any run) and `_` (one char), backslash escapes
    /// the next char (so `\%` matches a literal `%`).
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        /// v7.25 (mailrs round-17) — `ILIKE`: case-insensitive
        /// match. PG folds both operands.
        case_insensitive: bool,
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
        /// v7.24.1 — third slot: explicit NULLS FIRST/LAST
        /// (None = PG default, same contract as [`OrderBy`]).
        order_by: Vec<(
            Expr,
            bool,         /* desc */
            Option<bool>, /* nulls_first */
        )>,
        /// v4.20 explicit frame. `None` means "use the default":
        /// whole-partition when unordered, running aggregate from
        /// partition start through current row when ordered.
        frame: Option<WindowFrame>,
        /// v6.4.2 — `IGNORE NULLS` / `RESPECT NULLS` modifier on
        /// LAG / LEAD / FIRST_VALUE / LAST_VALUE. Default is
        /// `Respect` (PG / ANSI default — NULLs participate). Other
        /// window functions ignore this flag.
        null_treatment: NullTreatment,
        /// v7.37 D.40 — `agg(...) FILTER (WHERE cond) OVER (...)`. `None`
        /// = no FILTER. Only aggregate window functions honor it; the
        /// predicate restricts which peer rows contribute within the frame.
        filter: Option<Box<Expr>>,
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
    /// v7.30.2 (mailrs round-25) — `expr [NOT] IN (a, b, …)` as a FLAT
    /// list. Both the parser's literal-list path and the engine's
    /// IN-subquery materialisation used to desugar into a left-deep
    /// OR-Eq chain, so expression depth scaled with the element count
    /// — a 24k-row subquery result overflowed the 2 MiB worker stack
    /// (recursive eval AND recursive Box drop) and aborted embedding
    /// host processes. The flat node keeps depth constant: eval is an
    /// iterative scan with PG three-valued logic, drop is a Vec drop.
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
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
    /// Array slice `arr[lo:hi]` — PG 1-based, both ends
    /// inclusive; a missing bound extends to that end of the
    /// array and out-of-range bounds clamp. Returns an array of
    /// the same element type.
    ArraySlice {
        target: Box<Expr>,
        lo: Option<Box<Expr>>,
        hi: Option<Box<Expr>>,
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
    /// v7.37 (scout round 12) — `EXCLUDE {CURRENT ROW | GROUP |
    /// TIES | NO OTHERS}` frame exclusion. NO OTHERS is the default
    /// no-op; CURRENT ROW drops the current row from the frame.
    pub exclude: FrameExclusion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameExclusion {
    /// Default — exclude nothing.
    #[default]
    NoOthers,
    /// Drop the current row from the frame.
    CurrentRow,
    /// Drop the current row's whole peer group.
    Group,
    /// Drop the current row's peers but keep the current row.
    Ties,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Rows,
    Range,
    /// v7.37.19 (19.11) — PG 11+ `GROUPS BETWEEN N PRECEDING AND M
    /// FOLLOWING` peer-group frame mode. With UNBOUNDED / CURRENT ROW
    /// bounds (no explicit integer offsets) GROUPS behaves identically
    /// to RANGE — both consult the peer-group of the current row.
    /// Integer offsets are not yet supported; the executor rejects
    /// them at run time.
    Groups,
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
    /// Seconds since 1970-01-01 00:00:00 UTC (PG returns numeric;
    /// SPG keeps the integer convention — truncated seconds).
    Epoch,
    /// Day of week, 0 = Sunday … 6 = Saturday.
    Dow,
    /// ISO day of week, 1 = Monday … 7 = Sunday.
    Isodow,
    /// Day of year, 1-366.
    Doy,
    /// ISO 8601 week number, 1-53.
    Week,
    /// ISO 8601 week-numbering year (pairs with `Week`).
    Isoyear,
    /// Quarter, 1-4.
    Quarter,
    /// Year divided by 10 (floor).
    Decade,
    /// Century — 2001-2100 is century 21.
    Century,
    /// Millennium — 2001-3000 is millennium 3.
    Millennium,
    /// Julian day number (truncated for timestamps).
    Julian,
    /// Seconds and fraction in milliseconds (ss·1000 + frac).
    Millisecond,
    /// UTC offset in seconds — SPG sessions run UTC, so 0.
    Timezone,
    /// Hour component of the UTC offset — 0.
    TimezoneHour,
    /// Minute component of the UTC offset — 0.
    TimezoneMinute,
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
            Self::Epoch => "EPOCH",
            Self::Dow => "DOW",
            Self::Isodow => "ISODOW",
            Self::Doy => "DOY",
            Self::Week => "WEEK",
            Self::Isoyear => "ISOYEAR",
            Self::Quarter => "QUARTER",
            Self::Decade => "DECADE",
            Self::Century => "CENTURY",
            Self::Millennium => "MILLENNIUM",
            Self::Julian => "JULIAN",
            Self::Millisecond => "MILLISECOND",
            Self::Timezone => "TIMEZONE",
            Self::TimezoneHour => "TIMEZONE_HOUR",
            Self::TimezoneMinute => "TIMEZONE_MINUTE",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// v7.18 — `::bytea`. Decodes the LHS Text via PG's hex form
    /// (`'\xdeadbeef'`) or escape form (`'\x05\x00'`); Bytes
    /// inputs pass through unchanged. Closes the mailrs D-pre #3
    /// reverse-acceptance gap — anywhere a PG schema writes
    /// `expr::bytea`, SPG now matches.
    Bytea,
    /// v7.37.5 ship triage — generic cast target for the long tail
    /// of PG type names the parser meets in `expr::TYPE` shapes that
    /// don't deserve their own enum variant. The engine routes these
    /// through `column_type_to_data_type` + the existing typed
    /// `coerce_value` dispatch, so adding a new PG type to SPG
    /// implicitly adds its cast-target form too — no parser change
    /// per type. The string carries the lowercase PG type ident
    /// (e.g. `"point"`, `"int4multirange"`); the engine errors with
    /// a clear message when the type isn't known.
    Named(String),
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
            Self::Bytea => "bytea",
            // v7.37.5 — `Self::Named` carries its own canonical name.
            Self::Named(name) => return f.write_str(name),
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
    /// TEXT[] value carried through the prepared-bind path
    /// (`= ANY($1)` has no column context to re-parse a `{a,b}`
    /// text form, so the array rides the AST natively).
    TextArray(Vec<Option<String>>),
    /// INT[] value carried through the prepared-bind path.
    IntArray(Vec<Option<i32>>),
    /// BIGINT[] value carried through the prepared-bind path.
    BigIntArray(Vec<Option<i64>>),
    /// `INTERVAL '<n> <unit> [<n> <unit> ...]'` — calendar-aware span.
    /// Three independent dimensions: `months` (variable-length;
    /// year/month), `days` (fixed 86400 seconds at non-DST, but
    /// preserved as its own dimension so `'1 day'` ≠ `'24 hours'`
    /// stays distinguishable), and `micros` (sub-day; can carry).
    /// `text` keeps the original spelling so Display round-trips
    /// byte-for-byte. v7.37.5 β added the `days` field for PG parity.
    Interval {
        months: i32,
        days: i32,
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
    /// v7.37.7 C.1.7 — PG / SQL standard integer modulo. Same
    /// precedence as Mul/Div; result type follows left operand.
    Mod,
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
    /// Bitwise OR `|` on integers.
    BitOr,
    /// Bitwise AND `&` on integers.
    BitAnd,
    /// Bitwise XOR `#` on integers and equal-length bit strings.
    BitXor,
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
    /// v7.37.6-A `json <@ sub_json` — contained-by. Returns BOOL;
    /// `a <@ b` is defined as `b @> a` (same semantics, swapped
    /// sides). Eval dispatch reuses `JsonContains` with swapped args.
    JsonContainedBy,
    /// v7.37.6-A `json ? key` — key-exists. RHS is TEXT;
    /// returns BOOL. For an object, true if `key` is an existing
    /// member name; for an array, true if any element is the string
    /// `key` (PG semantics).
    JsonKeyExists,
    /// v7.37.6-A `json ?| keys` — any-key-exists. RHS is TEXT[];
    /// returns BOOL.
    JsonKeysAny,
    /// v7.37.6-A `json ?& keys` — all-keys-exist. RHS is TEXT[];
    /// returns BOOL.
    JsonKeysAll,
    /// `jsonb #- path_text[]` — delete the value at a nested path.
    /// RHS is a PG text-array literal like `'{a,b}'`; returns JSONB.
    JsonDeletePath,
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
    /// Bitwise NOT `~` on integers.
    BitNot,
}

// --- Display impls (round-trip-safe) --------------------------------------

impl Statement {
    /// v7.18 — classify whether the statement is read-only at
    /// engine level. Used by `spg-sqlx`'s `SpgConnection` to
    /// route SELECT-shaped traffic through the fan-out
    /// `AsyncReadHandle` (no writer-lock contention) while
    /// keeping DML / DDL / TX-control on the single-writer path.
    ///
    /// The classification matches what
    /// `Engine::execute_readonly_with_cancel` accepts: anything
    /// that does NOT mutate catalog, statistics, session state,
    /// or transaction state. WaitForWalPosition is included
    /// (engine returns `Unsupported`, but the classification is
    /// semantically read-only — no mutation). Empty is excluded
    /// out of an abundance of caution — the no-op routes
    /// through the writer so any future side effect lands
    /// uniformly.
    ///
    /// **Not connection-state aware**. `SET LOCAL` / `RESET`
    /// affect session parameters and must run on the writer
    /// engine that owns the session state; they classify as
    /// writer-path here. Same for `BEGIN` / `COMMIT` /
    /// `ROLLBACK` / `SAVEPOINT` — transaction control is
    /// always writer-path.
    #[must_use]
    pub fn is_readonly(&self) -> bool {
        match self {
            Statement::Select(_)
            | Statement::CopyTo { .. }
            | Statement::Explain(_)
            | Statement::ShowTables
            | Statement::ShowDatabases
            | Statement::ShowCreateTable(_)
            | Statement::ShowIndexes(_)
            | Statement::ShowStatus
            | Statement::ShowVariables
            | Statement::ShowProcesslist
            | Statement::ShowColumns(_)
            | Statement::ShowUsers
            | Statement::ShowPublications
            | Statement::ShowSubscriptions
            | Statement::WaitForWalPosition { .. } => true,
            // Everything else mutates catalog, statistics,
            // session state, or transaction state — writer path.
            // Listed explicitly so a new Statement variant fails
            // the match exhaustiveness check and forces a
            // classification decision at add-site.
            Statement::Empty
            | Statement::DropTable { .. }
            | Statement::DropIndex { .. }
            | Statement::CreateTable(_)
            | Statement::CreateExtension(_)
            | Statement::DoBlock(_)
            | Statement::CreateIndex(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
            | Statement::Merge(_)
            | Statement::Begin
            | Statement::Commit
            | Statement::Rollback
            | Statement::Savepoint(_)
            | Statement::RollbackToSavepoint(_)
            | Statement::ReleaseSavepoint(_)
            | Statement::CreateUser(_)
            | Statement::DropUser(_)
            | Statement::AlterIndex(_)
            | Statement::AlterTable(_)
            | Statement::CreatePublication(_)
            | Statement::DropPublication(_)
            | Statement::CreateSubscription(_)
            | Statement::DropSubscription(_)
            | Statement::Analyze(_)
            | Statement::Truncate { .. }
            | Statement::CompactColdSegments
            | Statement::SetParameter { .. }
            | Statement::SetParameterList(_)
            | Statement::SetTransaction { .. }
            | Statement::ShowParameter(_)
            | Statement::ResetParameter(_)
            | Statement::CreateFunction(_)
            | Statement::CreateTrigger(_)
            | Statement::DropTrigger { .. }
            | Statement::DropFunction { .. }
            | Statement::CreateSequence(_)
            | Statement::AlterSequence(_)
            | Statement::DropSequence { .. }
            | Statement::CreateView(_)
            | Statement::DropView { .. }
            | Statement::CreateMaterializedView(_)
            | Statement::RefreshMaterializedView { .. }
            | Statement::DropMaterializedView { .. }
            | Statement::CreateType(_)
            | Statement::AlterTypeAddValue { .. }
            | Statement::DropType { .. }
            | Statement::CreateDomain(_)
            | Statement::DropDomain { .. }
            | Statement::CreateSchema { .. }
            | Statement::DropSchema { .. } => false,
        }
    }
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => Ok(()),
            Self::CopyTo { table, columns } => {
                write!(f, "COPY {table}")?;
                if let Some(cols) = columns {
                    write!(f, " ({})", cols.join(", "))?;
                }
                write!(f, " TO STDOUT")
            }
            Self::Truncate {
                tables,
                restart_identity,
                cascade,
            } => {
                f.write_str("TRUNCATE TABLE ")?;
                for (i, t) in tables.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(t)?;
                }
                if *restart_identity {
                    f.write_str(" RESTART IDENTITY")?;
                }
                if *cascade {
                    f.write_str(" CASCADE")?;
                }
                Ok(())
            }
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
                if let Some(sub) = &s.source_select {
                    write!(f, "({sub})")?;
                } else {
                    write!(f, "{}", quote_ident(&s.source))?;
                }
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
            Self::ShowDatabases => f.write_str("SHOW DATABASES"),
            Self::ShowCreateTable(t) => write!(f, "SHOW CREATE TABLE {}", quote_ident(t)),
            Self::ShowIndexes(t) => write!(f, "SHOW INDEXES FROM {}", quote_ident(t)),
            Self::ShowStatus => f.write_str("SHOW STATUS"),
            Self::ShowVariables => f.write_str("SHOW VARIABLES"),
            Self::ShowProcesslist => f.write_str("SHOW PROCESSLIST"),
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
            Self::SetTransaction { isolation } => {
                write!(f, "SET TRANSACTION ISOLATION LEVEL ")?;
                let name = match isolation {
                    IsolationLevel::ReadUncommitted => "READ UNCOMMITTED",
                    IsolationLevel::ReadCommitted => "READ COMMITTED",
                    IsolationLevel::RepeatableRead => "REPEATABLE READ",
                    IsolationLevel::Serializable => "SERIALIZABLE",
                };
                f.write_str(name)
            }
            Self::ShowParameter(name) => write!(f, "SHOW {name}"),
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
            Self::AlterTypeAddValue {
                type_name,
                label,
                if_not_exists,
                position,
            } => {
                write!(f, "ALTER TYPE {type_name} ADD VALUE ")?;
                if *if_not_exists {
                    write!(f, "IF NOT EXISTS ")?;
                }
                write!(f, "'{label}'")?;
                if let Some((is_before, anchor)) = position {
                    write!(f, " {} '{anchor}'", if *is_before { "BEFORE" } else { "AFTER" })?;
                }
                Ok(())
            }
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
            Self::CreateSchema {
                name,
                if_not_exists,
            } => {
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
        write!(
            f,
            "CREATE DOMAIN {} AS {}",
            quote_ident(&self.name),
            self.base_type
        )?;
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
            TypeKind::Composite { fields } => {
                f.write_str("(")?;
                for (i, (n, t)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} {}", quote_ident(n), t)?;
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
                write!(
                    f,
                    " OWNED BY {}.{}",
                    quote_ident(table),
                    quote_ident(column)
                )?;
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
            Self::Assert { condition, message } => {
                write!(f, "ASSERT {condition}")?;
                if let Some(m) = message {
                    write!(f, ", {m}")?;
                }
                Ok(())
            }
            Self::While { condition, body } => {
                writeln!(f, "WHILE {condition} LOOP")?;
                for s in body {
                    writeln!(f, "  {s};")?;
                }
                f.write_str("END LOOP")
            }
            Self::ForRange {
                var,
                start,
                end,
                reverse,
                body,
            } => {
                write!(f, "FOR {var} IN ")?;
                if *reverse {
                    f.write_str("REVERSE ")?;
                }
                writeln!(f, "{start}..{end} LOOP")?;
                for s in body {
                    writeln!(f, "  {s};")?;
                }
                f.write_str("END LOOP")
            }
            Self::Loop { body } => {
                writeln!(f, "LOOP")?;
                for s in body {
                    writeln!(f, "  {s};")?;
                }
                f.write_str("END LOOP")
            }
            Self::Exit { when } => {
                f.write_str("EXIT")?;
                if let Some(c) = when {
                    write!(f, " WHEN {c}")?;
                }
                Ok(())
            }
            Self::Continue { when } => {
                f.write_str("CONTINUE")?;
                if let Some(c) = when {
                    write!(f, " WHEN {c}")?;
                }
                Ok(())
            }
            Self::ExecuteDynamic { sql } => write!(f, "EXECUTE {sql}"),
            Self::ForQuery { var, query, body } => {
                writeln!(f, "FOR {var} IN ({query}) LOOP")?;
                for s in body {
                    writeln!(f, "  {s};")?;
                }
                f.write_str("END LOOP")
            }
            Self::ForExecute {
                var,
                sql_expr,
                body,
            } => {
                writeln!(f, "FOR {var} IN EXECUTE {sql_expr} LOOP")?;
                for s in body {
                    writeln!(f, "  {s};")?;
                }
                f.write_str("END LOOP")
            }
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
        write!(f, "{}", quote_ident(&self.name))?;
        // v7.37.6-B — `PARTITION OF parent <bounds>` child form has
        // no column list and no constraints; the table inherits its
        // columns from the parent at engine-DDL time.
        if let Some(spec) = &self.partition_of {
            write!(f, " PARTITION OF {} ", quote_ident(&spec.parent_name))?;
            return match &spec.bounds {
                PartitionOfBoundsAst::Range { lower, upper } => {
                    write!(f, "FOR VALUES FROM ({}) TO ({})", *lower, *upper)
                }
                PartitionOfBoundsAst::List { values } => {
                    f.write_str("FOR VALUES IN (")?;
                    for (i, v) in values.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{}", v)?;
                    }
                    f.write_str(")")
                }
                PartitionOfBoundsAst::Hash { modulus, remainder } => {
                    write!(
                        f,
                        "FOR VALUES WITH (MODULUS {}, REMAINDER {})",
                        modulus, remainder
                    )
                }
                PartitionOfBoundsAst::Default => f.write_str("DEFAULT"),
            };
        }
        f.write_str(" (")?;
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
        f.write_str(")")?;
        // v7.37.6-B — partition-parent suffix renders after the
        // closing column-list paren, before the optional MySQL
        // table-options tail (which Display doesn't currently emit).
        if let Some(spec) = &self.partition_by {
            f.write_str(" PARTITION BY ")?;
            match spec.kind {
                PartitionKindAst::Range => f.write_str("RANGE ")?,
                PartitionKindAst::List => f.write_str("LIST ")?,
                PartitionKindAst::Hash => f.write_str("HASH ")?,
            }
            f.write_str("(")?;
            for (i, col) in spec.key_columns.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(&quote_ident(col))?;
            }
            f.write_str(")")?;
        }
        Ok(())
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
        AlterTableTarget::SetColumnAutoIncrement { column, seq_name } => {
            // Round-trip-safe spelling: re-parsing this form lowers
            // back to SetColumnAutoIncrement (the nextval default is
            // how pg_dump says "serial").
            let seq = seq_name
                .clone()
                .unwrap_or_else(|| alloc::format!("{column}_seq"));
            write!(
                f,
                "ALTER COLUMN {} SET DEFAULT nextval('{seq}')",
                quote_ident(column)
            )
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
        AlterTableTarget::AttachPartition { child, bounds } => {
            write!(f, "ATTACH PARTITION {} ", quote_ident(child))?;
            match bounds {
                PartitionOfBoundsAst::Range { lower, upper } => {
                    write!(f, "FOR VALUES FROM ({}) TO ({})", *lower, *upper)
                }
                PartitionOfBoundsAst::List { values } => {
                    f.write_str("FOR VALUES IN (")?;
                    for (i, v) in values.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{}", v)?;
                    }
                    f.write_str(")")
                }
                PartitionOfBoundsAst::Hash { modulus, remainder } => {
                    write!(
                        f,
                        "FOR VALUES WITH (MODULUS {}, REMAINDER {})",
                        modulus, remainder
                    )
                }
                PartitionOfBoundsAst::Default => f.write_str("DEFAULT"),
            }
        }
        AlterTableTarget::DetachPartition {
            child,
            concurrently,
            finalize,
        } => {
            write!(f, "DETACH PARTITION {}", quote_ident(child))?;
            if *concurrently {
                f.write_str(" CONCURRENTLY")?;
            }
            if *finalize {
                f.write_str(" FINALIZE")?;
            }
            Ok(())
        }
        AlterTableTarget::AlterColumnSetDefault {
            column,
            default_expr,
        } => write!(
            f,
            "ALTER COLUMN {} SET DEFAULT {}",
            quote_ident(column),
            default_expr
        ),
        AlterTableTarget::AlterColumnDropDefault { column } => {
            write!(f, "ALTER COLUMN {} DROP DEFAULT", quote_ident(column))
        }
        AlterTableTarget::AlterColumnSetNotNull { column } => {
            write!(f, "ALTER COLUMN {} SET NOT NULL", quote_ident(column))
        }
        AlterTableTarget::AlterColumnDropNotNull { column } => {
            write!(f, "ALTER COLUMN {} DROP NOT NULL", quote_ident(column))
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
        // v7.30.1 (mailrs round-24 class audit) — the type position
        // must re-parse to the same ColumnDef: a user-defined type
        // reference and the MySQL inline ENUM / SET value lists all
        // lower `ty` to Text, so rendering `ty` lost them.
        write!(f, "{}", quote_ident(&self.name))?;
        if let Some(ut) = &self.user_type_ref {
            write!(f, " {}", quote_ident(ut))?;
        } else if let Some(variants) = &self.inline_enum_variants {
            write_variant_list(f, "ENUM", variants)?;
        } else if let Some(variants) = &self.inline_set_variants {
            write_variant_list(f, "SET", variants)?;
        } else {
            write!(f, " {}", self.ty)?;
        }
        if self.is_unsigned {
            f.write_str(" UNSIGNED")?;
        }
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
        // v7.30.1 (mailrs round-24 class audit) — inline PRIMARY KEY
        // is NOT lifted to a table-level constraint at parse time
        // (unlike UNIQUE / CHECK), so the WAL round trip of a
        // prepared CREATE TABLE silently dropped the primary key.
        if self.is_primary_key {
            f.write_str(" PRIMARY KEY")?;
        }
        // The parser accepts only CURRENT_TIMESTAMP here (stored as
        // now()), so that spelling is the lossless round trip.
        if self.on_update_runtime.is_some() {
            f.write_str(" ON UPDATE CURRENT_TIMESTAMP")?;
        }
        // v7.37.7 — render GENERATED ALWAYS AS (…) STORED so WAL
        // replay reconstructs the computed-column declaration. The
        // expression sits inside a single set of parens; STORED is
        // the only variant the parser accepts.
        if let Some(gen_expr) = &self.generated_stored_expr {
            write!(f, " GENERATED ALWAYS AS ({gen_expr}) STORED")?;
        }
        Ok(())
    }
}

/// v7.30.1 — `ENUM('a', 'b')` / `SET('a', 'b')` inline value-list
/// types (MySQL flavour; `ty` is Text underneath).
fn write_variant_list(f: &mut fmt::Formatter<'_>, kw: &str, variants: &[String]) -> fmt::Result {
    write!(f, " {kw}(")?;
    for (i, v) in variants.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "'{}'", v.replace('\'', "''"))?;
    }
    f.write_str(")")
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
        // v7.30.1 (mailrs round-24) — ON CONFLICT must survive the
        // Display round trip: WAL persistence renders the bind-final
        // AST through this impl, and a replayed bare INSERT turns a
        // legal upsert no-op into a UNIQUE violation that refuses to
        // open the catalog.
        if let Some(oc) = &self.on_conflict {
            write!(f, " {oc}")?;
        }
        write_returning(self.returning.as_deref(), f)?;
        Ok(())
    }
}

/// v7.30.1 (mailrs round-24) — render the ON CONFLICT clause the
/// parser produced, so the AST→SQL round trip preserves upsert
/// semantics (WAL replay depends on it).
impl fmt::Display for OnConflictClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ON CONFLICT")?;
        if let Some(name) = &self.constraint_name {
            write!(f, " ON CONSTRAINT {name}")?;
        }
        if !self.target_columns.is_empty() {
            f.write_str(" (")?;
            for (i, c) in self.target_columns.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(&quote_ident(c))?;
            }
            f.write_str(")")?;
        }
        match &self.action {
            OnConflictAction::Nothing => f.write_str(" DO NOTHING"),
            OnConflictAction::Update {
                assignments,
                where_,
            } => {
                f.write_str(" DO UPDATE SET ")?;
                for (i, (col, expr)) in assignments.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} = {expr}", quote_ident(col))?;
                }
                if let Some(w) = where_ {
                    write!(f, " WHERE {w}")?;
                }
                Ok(())
            }
        }
    }
}

/// v7.30.1 (mailrs round-24) — shared `RETURNING <projection>`
/// tail for the three DML Display impls.
fn write_returning(ret: Option<&[SelectItem]>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let Some(items) = ret else {
        return Ok(());
    };
    f.write_str(" RETURNING ")?;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
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
        write_returning(self.returning.as_deref(), f)?;
        Ok(())
    }
}

impl fmt::Display for DeleteStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DELETE FROM {}", quote_ident(&self.table))?;
        if let Some(w) = &self.where_ {
            write!(f, " WHERE {w}")?;
        }
        write_returning(self.returning.as_deref(), f)?;
        Ok(())
    }
}

impl fmt::Display for CteBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Select(s) => write!(f, "{s}"),
            Self::Insert(s) => write!(f, "{s}"),
            Self::Update(s) => write!(f, "{s}"),
            Self::Delete(s) => write!(f, "{s}"),
        }
    }
}

impl fmt::Display for SelectStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // v7.30.1 (mailrs round-24 class audit) — the WITH clause
        // must survive the round trip; a CTE-using statement
        // re-parsed without it references undefined tables.
        if !self.ctes.is_empty() {
            f.write_str("WITH ")?;
            if self.ctes.iter().any(|c| c.recursive) {
                f.write_str("RECURSIVE ")?;
            }
            for (i, cte) in self.ctes.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(&quote_ident(&cte.name))?;
                if !cte.column_overrides.is_empty() {
                    f.write_str(" (")?;
                    for (ci, c) in cte.column_overrides.iter().enumerate() {
                        if ci > 0 {
                            f.write_str(", ")?;
                        }
                        f.write_str(&quote_ident(c))?;
                    }
                    f.write_str(")")?;
                }
                write!(f, " AS ({})", cte.body)?;
            }
            f.write_str(" ")?;
        }
        write_bare_select(self, f)?;
        for (kind, peer) in &self.unions {
            f.write_str(match kind {
                UnionKind::Distinct => " UNION ",
                UnionKind::All => " UNION ALL ",
                UnionKind::Intersect => " INTERSECT ",
                UnionKind::IntersectAll => " INTERSECT ALL ",
                UnionKind::Except => " EXCEPT ",
                UnionKind::ExceptAll => " EXCEPT ALL ",
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
                match o.nulls_first {
                    Some(true) => f.write_str(" NULLS FIRST")?,
                    Some(false) => f.write_str(" NULLS LAST")?,
                    None => {}
                }
            }
        }
        // v7.30.1 (mailrs round-24 class audit) — WITH TIES only
        // exists in the FETCH FIRST spelling; rendering it as LIMIT
        // dropped the tie-extension semantics on replay. The parser
        // accepts OFFSET before FETCH, so keep that order here.
        if self.limit_with_ties {
            if let Some(o) = &self.offset {
                write!(f, " OFFSET {o}")?;
            }
            if let Some(n) = &self.limit {
                write!(f, " FETCH FIRST {n} ROWS WITH TIES")?;
            }
        } else {
            if let Some(n) = &self.limit {
                write!(f, " LIMIT {n}")?;
            }
            if let Some(o) = &self.offset {
                write!(f, " OFFSET {o}")?;
            }
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
    } else if s.group_by_all {
        // v7.30.1 (mailrs round-24 class audit) — the GROUP BY ALL
        // shortcut parses to group_by: None + this flag; dropping
        // it turned an aggregate query into a bare projection on
        // re-parse.
        f.write_str(" GROUP BY ALL")?;
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
                JoinKind::Right => write!(f, " RIGHT JOIN {}", j.table)?,
                JoinKind::FullOuter => write!(f, " FULL OUTER JOIN {}", j.table)?,
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
        // v7.30.1 (mailrs round-24 class audit) — the dynamic
        // table-ref shapes must round-trip: rendering only the
        // (synthetic) name turned LATERAL / unnest() /
        // generate_series() into references to nonexistent tables
        // on re-parse.
        if let Some(inner) = &self.lateral_subquery {
            write!(f, "LATERAL ({inner})")?;
            if let Some(a) = &self.alias {
                write!(f, " AS {}", quote_ident(a))?;
                // v7.37 D.28 — a derived table on the lateral_subquery channel
                // may carry `AS t(cols)` column aliases (e.g. `(VALUES …) t(g)`
                // lowers here). Rendering the alias without the column list lost
                // the column names on re-parse (a view body round-trips through
                // Display), so `SELECT g FROM the_view` failed ColumnNotFound.
                if !self.unnest_column_aliases.is_empty() {
                    f.write_str(" (")?;
                    for (i, c) in self.unnest_column_aliases.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        f.write_str(&quote_ident(c))?;
                    }
                    f.write_str(")")?;
                }
            }
            return Ok(());
        }
        if let Some(expr) = &self.unnest_expr {
            write!(f, "UNNEST({expr})")?;
            if let Some(a) = &self.alias {
                write!(f, " AS {}", quote_ident(a))?;
                if !self.unnest_column_aliases.is_empty() {
                    f.write_str(" (")?;
                    for (i, c) in self.unnest_column_aliases.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        f.write_str(&quote_ident(c))?;
                    }
                    f.write_str(")")?;
                }
            }
            return Ok(());
        }
        if let Some(args) = &self.generate_series_args {
            f.write_str("generate_series(")?;
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{a}")?;
            }
            f.write_str(")")?;
            if let Some(a) = &self.alias {
                write!(f, " AS {}", quote_ident(a))?;
            }
            return Ok(());
        }
        write!(f, "{}", quote_ident(&self.name))?;
        if let Some(seg) = self.as_of_segment {
            write!(f, " AS OF SEGMENT {seg}")?;
        }
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
                UnOp::BitNot => write!(f, "(~{expr})"),
            },
            Self::Cast { expr, target } => write!(f, "({expr}::{target})"),
            Self::AggregateOrdered {
                call,
                order_by,
                distinct,
                filter,
            } => {
                let fmt_order_by = |f: &mut fmt::Formatter<'_>| -> fmt::Result {
                    for (i, o) in order_by.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{}", o.expr)?;
                        if o.desc {
                            f.write_str(" DESC")?;
                        }
                        match o.nulls_first {
                            Some(true) => f.write_str(" NULLS FIRST")?,
                            Some(false) => f.write_str(" NULLS LAST")?,
                            None => {}
                        }
                    }
                    Ok(())
                };
                // Ordered-set aggregates (`percentile_cont(f) WITHIN
                // GROUP (ORDER BY x)`) render the in-parens args as the
                // direct argument and the sort spec under WITHIN GROUP —
                // not as an in-argument ORDER BY.
                let ordered_set = matches!(
                    call.as_ref(),
                    Expr::FunctionCall { name, .. }
                        if matches!(
                            name.to_ascii_lowercase().as_str(),
                            "percentile_cont" | "percentile_disc" | "mode"
                        )
                );
                if ordered_set {
                    write!(f, "{call} WITHIN GROUP (ORDER BY ")?;
                    fmt_order_by(f)?;
                    f.write_str(")")?;
                } else {
                    // `name([DISTINCT ]args [ORDER BY …])` — peel the
                    // inner call's parens to splice modifiers.
                    let inner = alloc::format!("{call}");
                    let body = inner.strip_suffix(')').unwrap_or(&inner);
                    let (head, args_part) = body.split_once('(').unwrap_or((body, ""));
                    write!(f, "{head}(")?;
                    if *distinct {
                        f.write_str("DISTINCT ")?;
                    }
                    write!(f, "{args_part}")?;
                    if !order_by.is_empty() {
                        f.write_str(" ORDER BY ")?;
                        fmt_order_by(f)?;
                    }
                    f.write_str(")")?;
                }
                if let Some(cond) = filter {
                    write!(f, " FILTER (WHERE {cond})")?;
                }
                Ok(())
            }
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
                case_insensitive,
            } => {
                let op = match (negated, case_insensitive) {
                    (false, false) => "LIKE",
                    (true, false) => "NOT LIKE",
                    (false, true) => "ILIKE",
                    (true, true) => "NOT ILIKE",
                };
                write!(f, "({expr} {op} {pattern})")
            }
            Self::Extract { field, source } => write!(f, "EXTRACT({field} FROM {source})"),
            Self::WindowFunction {
                name,
                args,
                partition_by,
                order_by,
                frame,
                null_treatment,
                filter,
            } => {
                write!(f, "{name}(")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(")")?;
                // v7.37 D.40 — `FILTER (WHERE …)` sits between the arg list and
                // OVER; it round-trips so a window body's Display re-parses.
                if let Some(cond) = filter {
                    write!(f, " FILTER (WHERE {cond})")?;
                }
                // v7.30.1 (mailrs round-24 class audit) — IGNORE
                // NULLS sits between the arg list and OVER; dropping
                // it reverted replayed queries to RESPECT NULLS.
                if matches!(null_treatment, NullTreatment::Ignore) {
                    f.write_str(" IGNORE NULLS")?;
                }
                f.write_str(" OVER (")?;
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
                    for (i, (e, desc, nulls_first)) in order_by.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{e}")?;
                        if *desc {
                            f.write_str(" DESC")?;
                        }
                        match nulls_first {
                            Some(true) => f.write_str(" NULLS FIRST")?,
                            Some(false) => f.write_str(" NULLS LAST")?,
                            None => {}
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
                        FrameKind::Groups => "GROUPS",
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
            Self::InList {
                expr,
                list,
                negated,
            } => {
                let kw = if *negated { " NOT IN (" } else { " IN (" };
                write!(f, "({expr}{kw}")?;
                for (i, e) in list.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{e}")?;
                }
                f.write_str("))")
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
            Self::ArraySlice { target, lo, hi } => {
                write!(f, "({target}[")?;
                if let Some(l) = lo {
                    write!(f, "{l}")?;
                }
                write!(f, ":")?;
                if let Some(h) = hi {
                    write!(f, "{h}")?;
                }
                write!(f, "])")
            }
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
            // PG external array form. Display round-trip re-enters
            // through the column-typed text coerce, same as pgwire.
            Self::TextArray(items) => {
                f.write_str("'{")?;
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    match it {
                        None => f.write_str("NULL")?,
                        Some(s) => {
                            f.write_str("\"")?;
                            for c in s.chars() {
                                match c {
                                    // array-element escapes
                                    '"' | '\\' => write!(f, "\\{c}")?,
                                    // the OUTER wrapper is a SQL string
                                    // literal — embedded quotes must
                                    // double, or the rendered form
                                    // (WAL replay parses it back) is
                                    // invalid SQL
                                    '\'' => f.write_str("''")?,
                                    _ => write!(f, "{c}")?,
                                }
                            }
                            f.write_str("\"")?;
                        }
                    }
                }
                f.write_str("}'")
            }
            Self::IntArray(items) => {
                f.write_str("'{")?;
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    match it {
                        None => f.write_str("NULL")?,
                        Some(n) => write!(f, "{n}")?,
                    }
                }
                f.write_str("}'")
            }
            Self::BigIntArray(items) => {
                f.write_str("'{")?;
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    match it {
                        None => f.write_str("NULL")?,
                        Some(n) => write!(f, "{n}")?,
                    }
                }
                f.write_str("}'")
            }
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
            Self::Mod => "%",
            Self::L2Distance => "<->",
            Self::InnerProduct => "<#>",
            Self::CosineDistance => "<=>",
            Self::Concat => "||",
            Self::BitOr => "|",
            Self::BitAnd => "&",
            Self::BitXor => "#",
            Self::JsonGet => "->",
            Self::JsonGetText => "->>",
            Self::JsonGetPath => "#>",
            Self::JsonGetPathText => "#>>",
            Self::JsonContains => "@>",
            Self::JsonContainedBy => "<@",
            Self::JsonKeyExists => "?",
            Self::JsonKeysAny => "?|",
            Self::JsonKeysAll => "?&",
            Self::JsonDeletePath => "#-",
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
                    with_ordinality: false,
                    generate_series_args: None,
                    lateral_subquery: None,
                    jsonb_each_text_arg: None,
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
            distinct_on: Vec::new(),
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
