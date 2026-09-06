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

/// `COPY … TO STDOUT` output format. `text` is PG's default
/// (tab-separated, `\N` nulls, backslash escapes); `csv` follows
/// RFC-4180-style quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyFormat {
    #[default]
    Text,
    Csv,
}

/// Options for `COPY … TO STDOUT [WITH] (…)`. Defaults reproduce the
/// bare `COPY … TO STDOUT` text-format behaviour, so an empty option
/// list is a no-op. `delimiter` / `null_str` / `quote` fall back to the
/// per-format defaults (text: `\t` / `\N`; csv: `,` / `` / `"`) when
/// unset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CopyOptions {
    pub format: CopyFormat,
    pub header: bool,
    pub delimiter: Option<char>,
    pub null_str: Option<String>,
    pub quote: Option<char>,
    /// v7.39 (round 247) — CSV `ESCAPE`: the character that precedes a
    /// quote (or itself) inside a quoted cell. Defaults to the quote
    /// character (PG's doubling behavior).
    pub escape: Option<char>,
    /// v7.39 (round 247) — CSV `FORCE_QUOTE (col, …)` / `FORCE_QUOTE *`:
    /// columns whose non-NULL cells always quote. `Some(vec![])` is the
    /// `*` spelling (every column).
    pub force_quote: Option<Vec<String>>,
    /// v7.39 (round 265) — CSV `FORCE_NOT_NULL (col, …)`: for these
    /// columns an UNQUOTED empty field reads as the empty string rather
    /// than NULL (probed). COPY FROM only.
    pub force_not_null: Option<Vec<String>>,
    /// v7.39 (round 265) — CSV `FORCE_NULL (col, …)`: for these columns
    /// a QUOTED empty field (`""`) also reads as NULL (probed). COPY
    /// FROM only.
    pub force_null: Option<Vec<String>>,
}

/// v7.39 (round 218) — FETCH / MOVE cursor direction. PG grammar: single-row
/// forms (NEXT / PRIOR / FIRST / LAST / ABSOLUTE n / RELATIVE n) return at
/// most one row; multi-row forms (bare n / ALL / FORWARD [n|ALL] /
/// BACKWARD [n|ALL]) stream a run. A negative bare/FORWARD count means
/// BACKWARD (normalized at execution).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorDirection {
    Next,
    Prior,
    First,
    Last,
    Absolute(i64),
    Relative(i64),
    /// Bare `FETCH n` / `FORWARD n` (negative = backward n).
    Count(i64),
    /// `ALL` / `FORWARD ALL`.
    All,
    Backward(i64),
    BackwardAll,
}

impl fmt::Display for CursorDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Next => f.write_str("NEXT"),
            Self::Prior => f.write_str("PRIOR"),
            Self::First => f.write_str("FIRST"),
            Self::Last => f.write_str("LAST"),
            Self::Absolute(n) => write!(f, "ABSOLUTE {n}"),
            Self::Relative(n) => write!(f, "RELATIVE {n}"),
            Self::Count(n) => write!(f, "FORWARD {n}"),
            Self::All => f.write_str("ALL"),
            Self::Backward(n) => write!(f, "BACKWARD {n}"),
            Self::BackwardAll => f.write_str("BACKWARD ALL"),
        }
    }
}

/// v7.39 (round 320, V53) — what a `DISCARD` throws away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardTarget {
    All,
    Plans,
    Sequences,
    Temp,
}

impl fmt::Display for DiscardTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::All => "ALL",
            Self::Plans => "PLANS",
            Self::Sequences => "SEQUENCES",
            Self::Temp => "TEMP",
        })
    }
}

/// v7.39 (round 535) — which maintenance statement, and therefore what
/// its target names. Measured on PG18: INDEX / TABLE / CLUSTER name a
/// relation, SCHEMA names a schema, and SYSTEM / DATABASE name neither
/// in a way SPG can refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintainKind {
    ReindexRelation,
    ReindexSchema,
    /// `REINDEX SYSTEM` / `REINDEX DATABASE`, and a bare `CLUSTER`.
    Whole,
    ClusterRelation,
}

/// v7.39 (round 547) — see [`Statement::SetDbRoleSetting`]. Boxed in the
/// enum so the variant costs one pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetDbRoleSettingStatement {
    pub database: Option<String>,
    pub role: Option<String>,
    pub param: Option<String>,
    pub value: Option<String>,
}

/// v7.39 (round 696) — which operand a [`Statement::ValidateOnly`] names,
/// and therefore which catalog answers whether it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidateOnlyKind {
    /// `LOCK TABLE <t> [, …]` — the relation must exist.
    LockTable,
    /// Every role named must exist: `DROP OWNED BY <r> [, …]`,
    /// `REASSIGN OWNED BY <r> [, …] TO <r>`, and (round 697)
    /// `SET SESSION AUTHORIZATION <r>`.
    RoleName,
    /// `SECURITY LABEL …` — PG refuses unconditionally, because no label
    /// provider is loaded. SPG has none either.
    SecurityLabel,
    /// v7.39 (round 697) — `CREATE EXTENSION <e>`: the extension must be
    /// AVAILABLE (PG: `extension "x" is not available`).
    ExtensionAvailable,
    /// v7.39 (round 708) — `ALTER TYPE <t> <any no-op form>`: the TYPE must
    /// exist (PG: `type "x" does not exist`); the action itself stays a
    /// no-op (PG genuinely renames; that residual is recorded).
    TypeName,
    /// v7.39 (round 708) — `ALTER AGGREGATE name(args) …`: names[0] is the
    /// aggregate, the rest its argument type names (`*` = the `(*)` form).
    /// Existence only; the action no-ops (PG really renames built-ins —
    /// measured — and SPG does not model that).
    AggregateName,
    /// v7.39 (round 708) — `DROP CONVERSION <c>`: SPG ships no conversions,
    /// so every name answers PG's `conversion "x" does not exist`.
    ConversionName,
    /// v7.39 (round 708) — `DROP LANGUAGE <l>`: an unknown language does
    /// not exist; a shipped one is required (PG's two wordings, measured).
    LanguageName,
    /// v7.39 (round 709) — a collation name: performable or PG's
    /// `collation "x" for encoding "UTF8" does not exist`.
    CollationName,
    /// v7.39 (round 709) — a text search configuration name.
    TsConfigName,
    /// v7.39 (round 709) — an event trigger name. SPG has none, so the
    /// not-found answer is total.
    EventTriggerName,
    /// v7.39 (round 709) — a tablespace name. SPG has none beyond PG's two
    /// built-ins, whose drop PG refuses with `permission denied` (measured).
    TablespaceName,
    /// v7.39 (round 709) — a large-object oid (names[0], decimal). The
    /// registry is real (round 287), so the check is a lookup.
    LargeObjectOid,
    /// v7.39 (round 706) — `CREATE SERVER` / `CREATE FOREIGN TABLE` /
    /// `CREATE FOREIGN DATA WRAPPER`. SPG has no foreign-data
    /// infrastructure at all, so PG's refusals (`foreign-data wrapper "x"
    /// does not exist`, `server "x" does not exist`) cannot be copied —
    /// PG can refuse because the missing piece is installable there.
    /// Accepted with a WARNING, the extension resolution (round 697):
    /// refusing turns a dump that restores today into one that needs
    /// editing, and silent acceptance was the actual defect.
    ForeignInfra,
    /// v7.39 (round 697) — `DROP EXTENSION <e>`: it must be installed
    /// (PG: `extension "x" does not exist`).
    ExtensionInstalled,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // Statement::Select dominates; Boxing would touch every match site
pub enum Statement {
    /// v7.39 (round 695) — `ALTER SYSTEM SET <name> = …` / `RESET <name>`.
    ///
    /// It used to be swallowed with the rest of the ALTER no-ops, which meant
    /// `ALTER SYSTEM SET nosuch_guc = 1` was ACCEPTED where PG18 answers
    /// `unrecognized configuration parameter`. SPG still applies nothing —
    /// there is no postgresql.auto.conf to write — but a name it does not
    /// know is now refused rather than swallowed.
    ///
    /// `None` is `RESET ALL`, which names no parameter.
    AlterSystem {
        parameter: Option<String>,
    },
    /// `DROP DATABASE [IF EXISTS] <name>`. SPG is single-database, so
    /// this never succeeds; the name and the flag are carried so the
    /// engine can answer with PG's wording for the two cases PG itself
    /// has — an unknown name, or the database you are connected to.
    DropDatabase {
        name: String,
        if_exists: bool,
    },
    /// A statement SPG accepts as a no-op but PG refuses inside a
    /// transaction block — today `CREATE DATABASE` / `DROP DATABASE`,
    /// which are no-ops here because SPG is single-database.
    ///
    /// The no-op path they used to share (`Statement::Empty`) also
    /// carries CREATE ROLE, CREATE CAST and a dozen others that PG is
    /// happy to run inside a transaction, so the object has to be named
    /// to refuse the right ones.
    NoOpPreventedInTransaction {
        what: String,
        /// v7.38.18 — `CREATE DATABASE … LC_COLLATE 'de_DE.utf8'` is in
        /// every PostgreSQL bootstrap script there is, and SPG threw the
        /// whole statement away. Being single-database makes the NAME a
        /// no-op; it does not make the collation one, and a database
        /// that quietly sorts by the container's `LANG` instead of the
        /// one the script asked for is a silent difference in every
        /// `ORDER BY` it will ever run.
        ///
        /// `LOCALE` and `LC_COLLATE` both land here; `LC_CTYPE` does
        /// not, because SPG has no separate ctype.
        collation: Option<String>,
        /// v7.38.19 — the database's name, so `pg_database` can list one
        /// that was created and can be connected to. It was thrown away
        /// with the rest of the statement.
        name: Option<String>,
    },
    /// v7.39 (round 696) — statements SPG performs nothing for, but whose
    /// OPERAND PG validates before performing nothing either.
    ///
    /// All four used to be consumed whole by `is_dump_noise_statement`,
    /// which meant `LOCK TABLE nosuch` and `DROP OWNED BY nosuchrole` were
    /// ACCEPTED where PG18 errors. Accepting a statement that names
    /// something that does not exist is the F29 shape: the caller is told
    /// their intent was understood when the object it referred to is not
    /// there.
    ///
    /// They share one variant because they share one rule — resolve the
    /// name, refuse if absent, otherwise no-op — and four variants would be
    /// four places for that rule to drift.
    /// v7.39 (round 707) — `DROP AGGREGATE [IF EXISTS] name(argtypes)[, …]`.
    /// Consumed whole by the dump-noise list before, so `DROP AGGREGATE
    /// nosuch(int)` reported success. PG validates every named aggregate's
    /// EXISTENCE first (measured: a list with one unknown fails on the
    /// unknown even when an earlier entry exists), renders the signature
    /// with canonical type names (`int` → `integer`), and refuses to drop a
    /// built-in (`cannot drop function sum(integer) because it is required
    /// by the database system`). Every SPG aggregate is a built-in, so the
    /// outcome is one of those two errors — or the IF EXISTS no-op.
    ///
    /// `args` holds the argument type names as written; `None` is the
    /// `(*)` spelling.
    DropAggregate {
        if_exists: bool,
        items: Vec<(String, Option<Vec<String>>)>,
    },
    /// v7.39 (round 750) — `ALTER ROLE|USER <name> … PASSWORD 'x' |
    /// PASSWORD NULL`. The one attribute of the no-op family with a
    /// SECURITY consequence: it was silently dropped (ledgered r710),
    /// so a rotated credential never rotated. `None` = PASSWORD NULL
    /// (the role keeps existing but can no longer password-auth).
    AlterRolePassword {
        name: String,
        password: Option<String>,
    },
    ValidateOnly {
        kind: ValidateOnlyKind,
        /// The names the statement referred to. Empty means the form names
        /// nothing (`SECURITY LABEL`, whose refusal is unconditional).
        names: Vec<String>,
    },

    /// v7.39 (round 547) — `ALTER ROLE … SET/RESET` and
    /// `ALTER DATABASE … SET/RESET`: the GUC defaults a session picks up
    /// when it starts. Both used to land in the pg_dump no-op tail, so
    /// the statement reported success and changed nothing.
    ///
    /// `database` / `role` are `None` for PG's oid 0 — `ALTER ROLE ALL`
    /// sets both to None. `param` is `None` for RESET ALL. `value` is
    /// `None` for RESET of one parameter.
    SetDbRoleSetting(Box<SetDbRoleSettingStatement>),
    /// v7.39 (round 288) — `SET CONSTRAINTS { ALL | <name>… }
    /// { DEFERRED | IMMEDIATE }`. `deferred` carries the timing; the
    /// name list is not yet honoured (ALL is what pg_dump emits and
    /// what a circular-FK restore needs), so a named form applies to
    /// all deferrable constraints too rather than silently doing
    /// nothing.
    /// v7.39 (round 308) — `SET CONSTRAINTS { ALL | name [, …] }
    /// { DEFERRED | IMMEDIATE }`. An empty `names` is the ALL form;
    /// otherwise the timing applies only to the constraints listed.
    SetConstraints {
        names: Vec<String>,
        deferred: bool,
    },

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
        /// v7.39.7 — the table named by MySQL's `DROP INDEX i ON t`.
        ///
        /// MySQL keys an index name inside its table and its statement
        /// says so; PostgreSQL keys it in the schema and has no `ON`
        /// clause at all. `None` is the PostgreSQL form, which searches
        /// every table for the name, and is what the MySQL dialect
        /// refuses — as MySQL does.
        table: Option<String>,
    },
    /// v7.14.0 — empty / comment-only statement. The lexer strips
    /// `--` line comments and `/* … */` block comments (including
    /// the MySQL conditional `/*!NNNNN … */` form) before the
    /// parser ever sees them; a SQL chunk that contains nothing
    /// else lands here. Engine returns CommandOk no-op so
    /// pg_dump / mysqldump preambles (`SET NAMES utf8mb4`
    /// wrapped in conditional comments, etc.) load cleanly.
    /// v7.39 (round 277) — SQL-level `PREPARE <name> [(type, …)] AS
    /// <stmt>`. Session-scoped; the body keeps its `$N` placeholders
    /// and is substituted at EXECUTE time.
    Prepare {
        name: String,
        /// Declared parameter type names, in order. Empty when the
        /// `(type, …)` list was omitted (PG infers them).
        param_types: Vec<String>,
        body: alloc::boxed::Box<Statement>,
        /// The statement's own source text, which
        /// `pg_prepared_statements.statement` reports verbatim.
        source: String,
    },
    /// v7.39 (round 277) — `EXECUTE <name> [(arg, …)]`.
    Execute {
        name: String,
        args: Vec<Expr>,
    },
    /// v7.39 (round 277) — `DEALLOCATE {<name> | ALL}`. `None` = ALL.
    Deallocate(Option<String>),
    /// v7.39 (round 280) — `CREATE STATISTICS [IF NOT EXISTS] <name>
    /// [(kind, …)] ON <col>, … FROM <table>`. SPG records the object so
    /// dumps restore and reflection is honest; the planner does not
    /// consult it yet.
    CreateStatistics {
        name: String,
        if_not_exists: bool,
        /// Requested kinds as PG's single letters (`d` ndistinct,
        /// `f` dependencies, `m` mcv). Empty = PG's default set.
        kinds: Vec<String>,
        columns: Vec<String>,
        table: String,
    },
    /// v7.39 (round 280) — `DROP STATISTICS [IF EXISTS] <name>`.
    DropStatistics {
        name: String,
        if_exists: bool,
    },
    /// v7.39 (round 278) — `CALL <proc>(…)`. Parses; the engine
    /// reports that the procedure does not exist, because SPG has no
    /// procedure catalog. Carried as a statement rather than raised at
    /// parse time so the failure is a missing OBJECT (42883), not a
    /// syntax error.
    Call(String),
    /// v7.39 (round 278) — `PREPARE TRANSACTION '<gid>'`. Same shape:
    /// 2PC is unavailable, which PG itself reports when
    /// `max_prepared_transactions` is 0.
    PrepareTransaction(String),
    Empty,
    /// v7.39 (round 218) — `DECLARE <name> [BINARY] [INSENSITIVE]
    /// [[NO] SCROLL] CURSOR [{WITH|WITHOUT} HOLD] FOR <select>`. The
    /// canonical driver path for streaming large result sets (psycopg2
    /// named cursors, JDBC setFetchSize).
    DeclareCursor {
        name: String,
        /// `None` = neither keyword (PG default: backward allowed when the
        /// plan supports it — always, for SPG's materialized cursors);
        /// `Some(true)` = SCROLL; `Some(false)` = NO SCROLL (backward
        /// fetch errors 55000).
        scroll: Option<bool>,
        /// `WITH HOLD` — survives the creating transaction's COMMIT.
        hold: bool,
        query: Box<Statement>,
    },
    /// v7.39 (round 218) — `FETCH [<direction>] [FROM|IN] <name>`.
    FetchCursor {
        name: String,
        direction: CursorDirection,
    },
    /// v7.39 (round 218) — `MOVE [<direction>] [FROM|IN] <name>`: FETCH
    /// without returning rows; the command tag carries the move count.
    MoveCursor {
        name: String,
        direction: CursorDirection,
    },
    /// v7.39 (round 218) — `CLOSE <name>` / `CLOSE ALL` (`None` = ALL).
    CloseCursor {
        name: Option<String>,
    },
    /// v7.39 (round 222) — `LISTEN <channel>`: subscribe this session to
    /// async notifications on the channel.
    Listen(String),
    /// v7.39 (round 222) — `NOTIFY <channel> [, '<payload>']`. Delivered at
    /// COMMIT (PG semantics: transactional, deduplicated within the tx);
    /// immediately under autocommit.
    Notify {
        channel: String,
        payload: Option<String>,
    },
    /// v7.39 (round 222) — `UNLISTEN <channel>` / `UNLISTEN *` (`None` = *).
    Unlisten(Option<String>),
    /// `COPY table [(cols)] TO STDOUT` — the engine renders the
    /// visible rows in COPY text format (tab-separated, `\N`
    /// nulls, backslash escapes) as a single-text-column result
    /// set; the wire layer streams CopyData from it.
    CopyTo {
        table: String,
        columns: Option<Vec<String>>,
        /// v7.39 (read01 round 94) — `COPY (<query>) TO STDOUT`: an
        /// arbitrary SELECT/VALUES/CTE (a whole [`Statement`], so set-ops and
        /// VALUES ride through unchanged) whose result set is streamed in COPY
        /// format. `Some` overrides `table`/`columns` (which are empty then);
        /// `None` is the classic `COPY <table> …` shape.
        query: Option<Box<Statement>>,
        /// v7.37.x — `WITH (FORMAT csv, HEADER, DELIMITER, NULL, QUOTE)`
        /// and the legacy `WITH CSV HEADER …` spelling. Default =
        /// text format, no header (bare `COPY … TO STDOUT`).
        options: CopyOptions,
    },
    /// v7.39 (round 249) — `COPY table [(cols)] FROM '<path>' [(opts)]`.
    /// The engine is no_std and cannot read the file itself: the host
    /// (embedded / server / tooling) reads the path and hands the bytes to
    /// `Engine::copy_from_buffer`. Dispatching this statement straight to
    /// the engine reports that contract.
    CopyFromFile {
        table: String,
        columns: Option<Vec<String>>,
        path: String,
        options: CopyOptions,
    },
    /// v7.39 (round 249/252) — `COPY <table> [(cols)] TO '<file>'` (and
    /// the `COPY (<query>) TO '<file>'` form). The engine is no_std and
    /// cannot write the file itself: the host renders the payload via
    /// `Engine::copy_to_buffer` and writes the path.
    CopyToFile {
        table: String,
        columns: Option<Vec<String>>,
        query: Option<Box<Statement>>,
        path: String,
        options: CopyOptions,
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
    /// v7.39 (round 169) — `VACUUM [(opts)] [FULL|FREEZE|VERBOSE|ANALYZE]
    /// [<table>]`. Was a parse-time no-op from the pre-MVCC era; with the
    /// in-place MVCC gate ON, tombstoned versions are REAL bloat and a
    /// customer's manual VACUUM must actually reclaim. `analyze` mirrors
    /// the `VACUUM ANALYZE` spelling.
    Vacuum {
        table: Option<String>,
        analyze: bool,
    },
    /// `BEGIN` / `START TRANSACTION` — with an optional explicit
    /// `ISOLATION LEVEL …` mode (`None` = use the session default). PG
    /// applies the level for the duration of this transaction only.
    Begin(TransactionModes),
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
    /// v7.39.2 — MySQL `USE <db>`.
    ///
    /// It parsed as `Empty` and did nothing at all, so `USE myapp;
    /// SELECT DATABASE()` answered the same constant it answered before
    /// — measured against MySQL 9.7.2, which answers `myapp`. SPG serves
    /// ONE database and answers to any name (see `CREATE DATABASE`), so
    /// this does not switch catalogs; it records the NAME, which is the
    /// half a client can observe and the half the PostgreSQL wire has
    /// tracked since v7.39 (`current_database()` names what the startup
    /// message asked for).
    UseDatabase(String),
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
    /// r1067 — MySQL `SHOW VARIABLES LIKE 'pattern'` (sysbench-tpcc
    /// probes isolation with it at connect).
    ShowVariablesLike(String),
    /// v7.17.0 Phase 3.P0-62 — MySQL `SHOW PROCESSLIST`.
    ShowProcesslist,
    /// v7.39 (round 320, V53) — `DISCARD { ALL | PLANS | SEQUENCES | TEMP }`.
    /// pgbouncer sends `DISCARD ALL` between pooled client sessions to make
    /// the connection look brand new to the next client; it used to be
    /// swallowed as dump noise, so nothing was discarded.
    Discard(DiscardTarget),
    /// v7.39 (round 318, V51) — MySQL `KILL [CONNECTION | QUERY] <expr>`.
    /// The id is an expression because MariaDB accepts one
    /// (`KILL connection_id()` is the documented way to drop your own
    /// connection). `query_only` is the `QUERY` form: stop the target's
    /// running statement but leave it connected.
    Kill {
        query_only: bool,
        id: Box<Expr>,
    },
    /// `SHOW COLUMNS FROM <table>` — return one row per column with
    /// its declared name / type / nullability.
    ShowColumns(String),
    /// `CREATE USER 'name' WITH PASSWORD 'pw' ROLE 'admin'` (v4.1).
    /// Role is optional; defaults to `readonly` when omitted.
    CreateUser(CreateUserStatement),
    /// `DROP USER 'name'` (v4.1). v7.39 (read01 round 58) — `IF EXISTS` is
    /// carried through: PG skips with a NOTICE rather than erroring.
    DropUser {
        name: String,
        if_exists: bool,
    },
    /// v7.39 (RLS) — `SET ROLE { name | NONE | DEFAULT }` / `RESET ROLE`.
    /// `Some(name)` switches the session's effective role (drives
    /// `current_user` and RLS enforcement); `None` resets to the login
    /// identity (the Admin superuser).
    SetRole(Option<String>),
    /// v7.39 (read01 round 57) — `GRANT <privs> ON <object> TO <roles>`.
    Grant(GrantStatement),
    /// v7.39 (read01 round 57) — `REVOKE [GRANT OPTION FOR] <privs> ON
    /// <object> FROM <roles>`.
    Revoke(GrantStatement),
    /// v7.39 (RLS) — `CREATE POLICY name ON table …`.
    CreatePolicy(CreatePolicyStatement),
    /// v7.39 (RLS) — `ALTER POLICY name ON table …`.
    AlterPolicy(AlterPolicyStatement),
    /// v7.39 (RLS) — `DROP POLICY [IF EXISTS] name ON table`.
    DropPolicy(DropPolicyStatement),
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
    DropPublication {
        name: String,
        /// v7.39 (round 754, F31-B4) — `IF EXISTS` quietly skips a
        /// missing publication; the bare form refuses with PG's
        /// sentence (PG18-measured — the old "silent no-op" note on
        /// the executor was wrong).
        if_exists: bool,
    },
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
    DropSubscription {
        name: String,
        /// v7.39 (round 754, F31-B4) — same contract as
        /// [`Statement::DropPublication`].
        if_exists: bool,
    },
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
    /// v7.39.9 — MySQL's top-level `RENAME TABLE a TO b [, c TO d]`.
    /// PostgreSQL has only `ALTER TABLE … RENAME TO`, so this spelling
    /// had nowhere to go; it is what a MySQL migration writes.
    RenameTables(Vec<(String, String)>),
    /// v7.39 (round 535) — `REINDEX { INDEX | TABLE | SCHEMA | DATABASE
    /// | SYSTEM } [CONCURRENTLY] <name>` and `CLUSTER [VERBOSE]
    /// [<table> [USING <index>]]`.
    ///
    /// SPG has neither index bloat nor a clustering order to rebuild, so
    /// the work is a no-op — but PG VALIDATES the target, and both were
    /// swallowed at parse time, so `REINDEX TABLE typo` reported success.
    /// The name is carried now so the engine can say what PG says.
    Maintain {
        kind: MaintainKind,
        /// `REINDEX … CONCURRENTLY`. Carried for the same reason as
        /// [`CreateIndexStatement::concurrently`]: PG bars the
        /// CONCURRENTLY form inside a transaction block and allows the
        /// plain one.
        concurrently: bool,
        /// `None` for the whole-database forms, which name nothing.
        target: Option<String>,
    },
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
        /// v7.39 (round 647) — `TRUNCATE ONLY t`. Absorbed as a no-op
        /// since v7.14 on the reasoning that SPG's children are separate
        /// relations a truncate does not descend into. Same reasoning
        /// round 621 applied to `FROM ONLY`, and it stopped being true
        /// for the same reason: measured, `TRUNCATE <inheritance parent>`
        /// leaves the children's rows where PG empties them, and
        /// `TRUNCATE ONLY <partitioned parent>` is silently accepted
        /// where PG refuses it outright.
        only: bool,
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
        /// v7.38 (read01 P3.19) — `SET LOCAL` scopes the change to the
        /// current transaction; the engine saves the prior value and
        /// restores it at COMMIT / ROLLBACK. Plain `SET` (and `SET
        /// SESSION`) leave this false and persist for the session.
        local: bool,
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
    /// v7.39 (round 430) — MySQL's USER-defined variables:
    /// `SET @x = 5, @s := CONCAT('a','b')`. Distinct from
    /// [`Self::SetParameter`] (a `@@`-style engine/session setting) in
    /// every way that matters: the value is an arbitrary EXPRESSION, the
    /// name lives in its own per-session namespace, and reading an unset
    /// one answers NULL rather than raising. `:=` and `=` are the same
    /// assignment here.
    ///
    /// Before this the parser stripped every `@`, so `@x` and `@@x` were
    /// the same node: `SET @x = 5` silently landed in the session-parameter
    /// store where nothing could read it back, and `SELECT @x` failed with
    /// "Unknown system variable".
    /// v7.39 (round 554) — `SET @a = …, SETTING = …`.
    ///
    /// `settings` is the trailing half a mysqldump preamble writes:
    /// `SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO'`
    /// saves a value and changes it in one statement. The parser used
    /// to refuse the mixture outright, so no mysqldump could be
    /// restored past its preamble.
    SetUserVars(Vec<(String, Expr)>, Vec<(String, Expr)>),
    /// v7.38 轴 4 — `SET [SESSION] TRANSACTION ISOLATION LEVEL …`
    /// (plus optional READ ONLY / READ WRITE / DEFERRABLE clauses
    /// silently accepted). PG-standard surface for picking an
    /// isolation level. Engine tracks the value on
    /// `Engine::current_isolation_level()`; actual MVCC / SSI
    /// semantics implementation lands separately. PG itself maps
    /// READ UNCOMMITTED to READ COMMITTED; SPG mirrors that —
    /// effectively every level reads as READ COMMITTED in v7.37.8.
    SetTransaction {
        modes: TransactionModes,
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
    /// v7.39 (round 139) — `CREATE RULE name AS ON event TO table [WHERE cond]
    /// DO [ALSO|INSTEAD] { NOTHING | command }` query-rewrite rule.
    CreateRule(CreateRuleStatement),
    /// v7.39 (round 139) — `DROP RULE [IF EXISTS] name ON table`.
    DropRule {
        name: String,
        table: String,
        if_exists: bool,
    },
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
        /// v7.39 (read01 round 62) — the argument TYPES, when the statement gave
        /// them: `DROP FUNCTION f(int)` drops that overload only. `None` = no
        /// argument list, which PG accepts only when the name is unambiguous.
        args: Option<Vec<String>>,
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
    /// v7.39 (read01 round 49) — `ALTER TYPE t RENAME VALUE 'old' TO 'new'`.
    /// Used to be swallowed by the ALTER TYPE no-op tail, so the rename was
    /// accepted and silently ignored.
    /// v7.39 (read01 round 50) — `COMMENT ON <kind> <name> IS { 'text' | NULL }`.
    /// Used to be swallowed as dump noise, so a comment was accepted and lost
    /// (and obj_description / col_description always returned NULL).
    /// `kind` is lowercase ("table" / "column" / "index" / …); for a column
    /// `name` is the dotted `table.column`. `comment: None` = `IS NULL` = remove.
    CommentOn {
        kind: String,
        name: String,
        comment: Option<String>,
    },
    AlterTypeRenameValue {
        type_name: String,
        old: String,
        new: String,
    },
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
    /// v7.39 (round 260) — `ALTER DOMAIN name <action>`. Every form was
    /// previously swallowed by the catch-all DDL arm: the statement
    /// reported success and did nothing, so a migration that dropped a
    /// constraint kept rejecting the data it had just been told to
    /// accept.
    AlterDomain {
        name: String,
        action: AlterDomainAction,
    },
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

/// v7.39 (round 260) — the `ALTER DOMAIN` actions SPG implements.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterDomainAction {
    AddConstraint { name: Option<String>, check: Expr },
    DropConstraint { name: String, if_exists: bool },
    SetDefault(Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    RenameTo(String),
}

/// v7.17.0 Phase 1.5 — `CREATE DOMAIN` AST.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateDomainStatement {
    pub name: String,
    /// Base type for the domain (one of the built-in
    /// `ColumnTypeName` variants).
    pub base_type: ColumnTypeName,
    /// v7.39 (round 259) — `CREATE DOMAIN child AS parent …` where
    /// `parent` is itself a DOMAIN. The parser already captured the
    /// unknown type name; it just was not carried here, so the parent's
    /// CHECK constraints were invisible and a value violating them was
    /// silently accepted. `base_type` still holds the ultimate scalar
    /// type, which is what the storage tier stores.
    pub base_domain: Option<String>,
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
        /// v7.39 (round 264) — parallel to `fields`: the raw type NAME
        /// when a field's type is not a builtin (i.e. another composite).
        /// The parser already captures it; without carrying it here a
        /// nested composite field resolved to the Text placeholder and
        /// the inner record never became a record.
        field_user_types: Vec<Option<String>>,
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
    /// would return it. v7.39 (round 770, F31 tranche 6 #154) — PG
    /// KEEPS the "read uncommitted" label (measured: `BEGIN ISOLATION
    /// LEVEL READ UNCOMMITTED; SHOW transaction_isolation` answers
    /// `read uncommitted`) and only BEHAVES as read committed; the old
    /// fold renamed the label too.
    /// v7.39 — the MySQL display name, which is NOT the PG one: MySQL
    /// hyphenates and upper-cases. Measured on MySQL 9.7.2 by setting
    /// each level and reading `@@transaction_isolation` back:
    /// `READ-UNCOMMITTED` / `READ-COMMITTED` / `REPEATABLE-READ` /
    /// `SERIALIZABLE` (the last has no hyphen because it is one word).
    ///
    /// This exists so the two MySQL surfaces cannot drift: both
    /// `SHOW VARIABLES` and `@@transaction_isolation` used to carry
    /// their own hard-coded literal, and the literals disagreed —
    /// one said `REPEATABLE-READ` while the engine ran read committed.
    /// v7.39 — parse what `default_transaction_isolation` holds. PG
    /// accepts the SQL spellings and stores them lower-cased with a
    /// space; anything else is not a level this understands and the
    /// caller keeps its own default rather than guessing.
    #[must_use]
    pub fn from_pg_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "read uncommitted" => Some(Self::ReadUncommitted),
            "read committed" => Some(Self::ReadCommitted),
            "repeatable read" => Some(Self::RepeatableRead),
            "serializable" => Some(Self::Serializable),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_mysql_str(self) -> &'static str {
        match self {
            Self::ReadUncommitted => "READ-UNCOMMITTED",
            Self::ReadCommitted => "READ-COMMITTED",
            Self::RepeatableRead => "REPEATABLE-READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }

    pub fn as_pg_str(self) -> &'static str {
        match self {
            Self::ReadUncommitted => "read uncommitted",
            Self::ReadCommitted => "read committed",
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
    /// v7.38 (read01 P6.49) — when true this node came from
    /// `CREATE TABLE … AS <select>` (CTAS) / `SELECT … INTO`, so the
    /// executor creates a plain table and does NOT register it in the
    /// materialized-view registry (no REFRESH semantics).
    pub as_plain_table: bool,
    /// v7.39 (round 436) — `CREATE TEMPORARY TABLE … AS <select>`. Only
    /// meaningful together with `as_plain_table`; the executor puts the
    /// resulting table in the creating session's namespace.
    pub temporary: bool,
}

/// v7.39 (read01 round 132) — `WITH [LOCAL | CASCADED] CHECK OPTION` on an
/// auto-updatable view. `Cascaded` is PG's default when the bare
/// `WITH CHECK OPTION` is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewCheckOption {
    Local,
    Cascaded,
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
    /// v7.39 (round 132) — `WITH CHECK OPTION`. When set, a write through this
    /// view whose resulting row fails the view's WHERE is rejected (SQLSTATE
    /// 44000). `None` = no check option.
    pub check_option: Option<ViewCheckOption>,
}

/// v7.17.0 — `ALTER SEQUENCE` AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterSequenceStatement {
    pub name: String,
    pub if_exists: bool,
    pub options: SequenceOptions,
    /// v7.39 (read01 round 49) — `ALTER SEQUENCE old RENAME TO new`. Set
    /// instead of `options`; the two forms are mutually exclusive in PG.
    pub rename_to: Option<String>,
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
    /// v7.39 (round 754, F31-B5) — `FOR TABLES IN SCHEMA <name>`
    /// (PG 15+). AST-only: the executor folds `public` to
    /// [`PublicationScope::AllTables`] (SPG's single-schema world)
    /// and refuses any other schema with PG's sentence, so the
    /// catalog / serializer / replication filter never see it.
    TablesInSchema(String),
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
    /// v7.39 (round 710) — `SET ( option = value, … )` / `RESET ( … )`.
    /// Was a SYNTAX ERROR; PG resolves the INDEX first (`relation "x"
    /// does not exist`), so the index is validated and the storage
    /// parameters no-op (SPG engine-manages them, as ALTER TABLE's
    /// SET/RESET arms already record).
    StorageParams,
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
/// v7.39.9 — the `FIRST` / `AFTER c` trailer, written back the way it
/// was read.
fn write_column_position(
    f: &mut core::fmt::Formatter<'_>,
    pos: Option<&ColumnPosition>,
) -> core::fmt::Result {
    match pos {
        Some(ColumnPosition::First) => f.write_str(" FIRST"),
        Some(ColumnPosition::After(c)) => write!(f, " AFTER {}", quote_ident(c)),
        None => Ok(()),
    }
}

/// v7.39.9 — where MySQL's `ADD` / `MODIFY` / `CHANGE` puts a column.
///
/// The row encoding is positional and `SELECT *` reads it in order, so
/// this is an answer, not a formatting preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnPosition {
    First,
    After(String),
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum AlterTableTarget {
    /// v7.39 (round 647) — `ALTER TABLE c INHERIT p` / `NO INHERIT p`.
    ///
    /// Both were accepted-and-ignored since v7.37.18, on the reasoning
    /// that SPG has no PG-style inheritance. Round 645 gave it one, and
    /// the reasoning went stale: `NO INHERIT` reported success while the
    /// child stayed attached, which is the worst kind of answer — the
    /// statement says it worked and the catalog disagrees.
    Inherit { parent: String, detach: bool },
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
    /// v7.39 (round 431) — MySQL's `ALTER TABLE t DROP {INDEX|KEY} name`,
    /// the counterpart of `ADD INDEX`. Lowers to the same catalog action
    /// as the standalone `DROP INDEX` statement.
    DropIndex { name: String, if_exists: bool },
    /// v7.13.0 — `ALTER TABLE t ADD [COLUMN] [IF NOT EXISTS] <col>
    /// <type> [DEFAULT <expr>] [NOT NULL]`. mailrs round-5 G1
    /// (20 migrate-*.sql hits). Engine appends the column to the
    /// schema and back-fills every existing row with the DEFAULT
    /// (or NULL when no DEFAULT and the column is nullable).
    AddColumn {
        column: ColumnDef,
        if_not_exists: bool,
        /// v7.39.9 — MySQL's `FIRST` / `AFTER <col>`, which say where
        /// the column goes. `None` is the PostgreSQL form and appends.
        position: Option<ColumnPosition>,
    },
    /// v7.39.9 — MySQL's `MODIFY COLUMN c <definition>` and
    /// `CHANGE COLUMN old new <definition>`.
    ///
    /// Both REPLACE the column's definition rather than amending it,
    /// which is the part that cannot be expressed by the PostgreSQL
    /// spellings SPG already had. Measured on MySQL 9.7.2: a column
    /// declared `INT NOT NULL DEFAULT 5`, after `MODIFY COLUMN b
    /// BIGINT`, is `bigint` NULLABLE with NO default — restating them
    /// keeps them, omitting them drops them. `CHANGE` is the same and
    /// also renames.
    ModifyColumn {
        /// The column as it is named now.
        column: String,
        /// `CHANGE`'s new name; `None` for `MODIFY`, which keeps it.
        rename_to: Option<String>,
        /// The whole new definition, exactly as written.
        definition: ColumnDef,
        position: Option<ColumnPosition>,
    },
    /// v7.39.9 — MySQL's `RENAME {INDEX|KEY} old TO new`.
    RenameIndex { old: String, new: String },
    /// v7.39.9 — MySQL's `ALTER TABLE t AUTO_INCREMENT = n`, which sets
    /// the value the NEXT insert takes. Measured on 9.7.2: after
    /// `= 100`, the next row's id is 100.
    SetTableAutoIncrement(i64),
    /// v7.39.9 — MySQL's `ENGINE = <name>`. SPG has one storage engine
    /// and substitutes for every name MySQL knows, exactly as
    /// `CREATE TABLE` already does; a name MySQL does not know is
    /// refused with its 1286, because a typo in a migration must not
    /// quietly become SPG's storage.
    SetEngine(String),
    /// v7.39.9 — MySQL's `CONVERT TO CHARACTER SET <cs> [COLLATE <c>]`.
    /// SPG stores UTF-8 throughout, so a charset it can represent is
    /// accepted and one it cannot is refused with MySQL's 1115.
    ConvertToCharacterSet {
        charset: String,
        collate: Option<String>,
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
        /// v7.39 (round 713) — `COLLATE <name>` between the type and
        /// USING. PG re-collates the column, and an ABSENT clause RESETS
        /// the collation to the type default (measured round 713) — so
        /// `None` is not "leave it alone". The type parser consumed the
        /// clause all along and this surface dropped it on the floor:
        /// the statement succeeded and the ordering did not change, the
        /// silent-divergence shape. Folded variant + the name as written.
        collation: Option<(Collation, String)>,
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
    /// v7.39 (round 652) — `OWNER TO <role>`. SPG is single-owner, so
    /// there is nothing to record; what PG does that SPG did not is
    /// REFUSE a role that does not exist. The name has to reach the
    /// engine for that, because only the engine knows the roles.
    OwnerTo { role: String },
    /// v7.39 (round 652) — `CLUSTER ON <index>` and `SET WITHOUT
    /// CLUSTER` (the latter as `None`). SPG has no clustered storage, so
    /// the hint is still a no-op; naming an index that does not exist is
    /// not.
    ClusterOn { index: Option<String> },
    /// v7.39 (round 652) — `VALIDATE CONSTRAINT <name>`: scan the rows
    /// already in the table against a constraint added `NOT VALID` and,
    /// if they all pass, mark it validated. It used to be swallowed as a
    /// no-op on the theory that SPG validated at ADD time; SPG did not.
    ValidateConstraint { name: String },
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
    /// v7.39 (read01 round 48) — `ALTER TABLE t RENAME CONSTRAINT old TO new`.
    /// Reachable now that the schema stores user-supplied constraint names.
    RenameConstraint { old: String, new: String },
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
    /// v7.39 (RLS) — `ALTER TABLE t { ENABLE | DISABLE | FORCE | NO FORCE }
    /// ROW LEVEL SECURITY`. `enabled` = Some for ENABLE/DISABLE (sets
    /// `relrowsecurity`); `force` = Some for FORCE/NO FORCE (sets
    /// `relforcerowsecurity`). Exactly one is `Some` per statement.
    SetRowSecurity {
        enabled: Option<bool>,
        force: Option<bool>,
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
    AlterColumnSetDefault { column: String, default_expr: Expr },
    /// v7.37.18 (18.1) — `ALTER TABLE … ALTER COLUMN col DROP DEFAULT`.
    AlterColumnDropDefault { column: String },
    /// v7.37.18 (18.2) — `ALTER TABLE … ALTER COLUMN col SET NOT NULL`.
    /// Engine validates that no existing row has NULL in that column
    /// before flipping the flag (PG semantics — partial NOT NULL
    /// would surface inconsistently).
    AlterColumnSetNotNull { column: String },
    /// v7.37.18 (18.2) — `ALTER TABLE … ALTER COLUMN col DROP NOT NULL`.
    AlterColumnDropNotNull { column: String },
    /// v7.39 (round 220) — `ALTER TABLE … ALTER COLUMN col RESTART
    /// [WITH n]` on an identity column (`None` = bare RESTART, from the
    /// column's start value = 1). Engine records a next-value floor over
    /// SPG's max+1 identity allocation.
    AlterColumnRestart { column: String, with: Option<i64> },
    /// v7.38 (read01 U10) — `ALTER TABLE … ALTER COLUMN col DROP
    /// EXPRESSION` turns a stored generated column into a plain column
    /// (its generation expression is removed; existing values are kept).
    AlterColumnDropExpression { column: String, if_exists: bool },
    /// v7.38 (read01, T28) — `ALTER COLUMN col DROP IDENTITY [IF EXISTS]`:
    /// de-generate an identity column into a plain column.
    AlterColumnDropIdentity { column: String, if_exists: bool },
    /// v7.38 (read01 U12) — `ALTER TABLE … ALTER COLUMN col SET
    /// EXPRESSION AS (expr)` (PG 17) changes a stored generated column's
    /// expression and recomputes every existing row.
    AlterColumnSetExpression { column: String, expr: Expr },
    /// v7.39 (round 710) — `OF <type>` / the type half of the typed-table
    /// binding. The BINDING stays a no-op (recorded); the TYPE must exist
    /// (PG: `type "x" does not exist`).
    OfType { type_name: String },
    /// v7.39 (round 710) — `REPLICA IDENTITY USING INDEX <i>`. The
    /// identity setting no-ops (SPG has no logical replication consumer);
    /// the INDEX must exist on this table (PG: `index "i" for table "t"
    /// does not exist`).
    ReplicaIdentityUsingIndex { index: String },
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

/// Each bool mirrors one independent PG `EXPLAIN (…)` option (ANALYZE,
/// SUGGEST, COSTS OFF, BUFFERS, TIMING OFF, …); they compose freely, so a
/// bitflags word or a nested options struct would only relocate the lint
/// while making the option each caller sets harder to read.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct ExplainStatement {
    pub analyze: bool,
    /// v7.39 (round 225) — widened from SelectStatement so EXPLAIN
    /// INSERT/UPDATE/DELETE parses (PG explains DML); the engine renders
    /// `Insert on / Update on / Delete on` trees for them.
    pub inner: Box<Statement>,
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
    /// v7.39 (round 227) — `EXPLAIN (SUMMARY OFF)` suppresses the trailing
    /// `Planning Time:` / `Execution Time:` lines. PG defaults SUMMARY on
    /// for ANALYZE and off otherwise; SPG emits them for ANALYZE unless
    /// this is set.
    pub summary_off: bool,
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

/// v7.39 (RLS) — the command a `CREATE POLICY` scopes to. `All` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyCmd {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

/// v7.39 (RLS) — `CREATE POLICY name ON table [AS {PERMISSIVE|RESTRICTIVE}]
/// [FOR cmd] [TO roles] [USING (expr)] [WITH CHECK (expr)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatePolicyStatement {
    pub name: String,
    pub table: String,
    /// `true` = PERMISSIVE (default), `false` = RESTRICTIVE.
    pub permissive: bool,
    pub cmd: PolicyCmd,
    /// Empty = PUBLIC.
    pub roles: Vec<String>,
    pub using: Option<Expr>,
    pub with_check: Option<Expr>,
}

/// v7.39 (RLS) — `ALTER POLICY name ON table { RENAME TO new | [TO roles]
/// [USING (expr)] [WITH CHECK (expr)] }`. Cannot change PERMISSIVE/RESTRICTIVE
/// or the command (matches PG).
#[derive(Debug, Clone, PartialEq)]
pub struct AlterPolicyStatement {
    pub name: String,
    pub table: String,
    pub rename_to: Option<String>,
    pub roles: Option<Vec<String>>,
    pub using: Option<Expr>,
    pub with_check: Option<Expr>,
}

/// v7.39 (RLS) — `DROP POLICY [IF EXISTS] name ON table`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropPolicyStatement {
    pub name: String,
    pub table: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateUserStatement {
    pub name: String,
    /// Empty when the statement carried no PASSWORD — legal for a bare
    /// `CREATE ROLE`, which cannot log in anyway.
    pub password: String,
    /// One of `admin` / `readwrite` / `readonly`. Stored verbatim from
    /// the parser; the engine validates against `Role::parse` so a
    /// typo lands as a runtime error with a clear message rather than
    /// a parse failure.
    pub role: String,
    /// v7.39 (read01 round 58) — the PG role attributes. `None` = the
    /// statement did not say, so the default for its spelling applies:
    /// `CREATE USER` is `CREATE ROLE … LOGIN`, `CREATE ROLE` is NOLOGIN;
    /// both default to INHERIT and NOSUPERUSER.
    pub login: Option<bool>,
    pub inherit: Option<bool>,
    pub superuser: Option<bool>,
    /// `true` when spelled `CREATE USER` (LOGIN by default).
    pub is_user: bool,
}

/// v7.39 (round 322, V46) — PG's function volatility class. Declarative:
/// it tells the planner how far a call may be moved or folded. SPG records
/// it faithfully (`pg_proc.provolatile`, `pg_get_functiondef`) and does not
/// yet exploit it for constant folding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FunctionVolatility {
    Immutable,
    Stable,
    #[default]
    Volatile,
}

impl FunctionVolatility {
    /// PG's one-character `pg_proc.provolatile` code.
    #[must_use]
    pub const fn as_pg_char(self) -> &'static str {
        match self {
            Self::Immutable => "i",
            Self::Stable => "s",
            Self::Volatile => "v",
        }
    }

    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Immutable => "IMMUTABLE",
            Self::Stable => "STABLE",
            Self::Volatile => "VOLATILE",
        }
    }
}

/// v7.39 (round 322, V46) — PG's parallel-safety class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FunctionParallel {
    #[default]
    Unsafe,
    Restricted,
    Safe,
}

impl FunctionParallel {
    /// PG's one-character `pg_proc.proparallel` code.
    #[must_use]
    pub const fn as_pg_char(self) -> &'static str {
        match self {
            Self::Unsafe => "u",
            Self::Restricted => "r",
            Self::Safe => "s",
        }
    }

    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Unsafe => "PARALLEL UNSAFE",
            Self::Restricted => "PARALLEL RESTRICTED",
            Self::Safe => "PARALLEL SAFE",
        }
    }
}

/// v7.39 (round 322, V46) — the attribute clauses `CREATE FUNCTION` accepts
/// on either side of its body. Defaults are PG's: VOLATILE, called on null
/// input, SECURITY INVOKER, not leakproof, PARALLEL UNSAFE, and the
/// language's default cost / rows.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FunctionAttrs {
    pub volatility: FunctionVolatility,
    /// `STRICT` / `RETURNS NULL ON NULL INPUT`: a call with any NULL
    /// argument returns NULL without running the body.
    pub strict: bool,
    pub security_definer: bool,
    pub leakproof: bool,
    pub parallel: FunctionParallel,
    /// `COST n` — `None` leaves PG's per-language default.
    pub cost: Option<f64>,
    /// `ROWS n` — set-returning functions only; `None` = default.
    pub rows: Option<f64>,
}

impl FunctionAttrs {
    /// The attribute words `pg_get_functiondef` puts on their own line,
    /// in PG's order (measured on 18.4: volatility, PARALLEL, STRICT,
    /// SECURITY DEFINER, LEAKPROOF, COST, ROWS). Empty when everything is
    /// at its default — PG then emits no such line at all.
    #[must_use]
    pub fn render_words(&self) -> alloc::vec::Vec<alloc::string::String> {
        let mut out = alloc::vec::Vec::new();
        if self.volatility != FunctionVolatility::Volatile {
            out.push(alloc::string::String::from(self.volatility.as_sql()));
        }
        if self.parallel != FunctionParallel::Unsafe {
            out.push(alloc::string::String::from(self.parallel.as_sql()));
        }
        if self.strict {
            out.push(alloc::string::String::from("STRICT"));
        }
        if self.security_definer {
            out.push(alloc::string::String::from("SECURITY DEFINER"));
        }
        if self.leakproof {
            out.push(alloc::string::String::from("LEAKPROOF"));
        }
        if let Some(c) = self.cost {
            out.push(alloc::format!("COST {}", render_attr_number(c)));
        }
        if let Some(r) = self.rows {
            out.push(alloc::format!("ROWS {}", render_attr_number(r)));
        }
        out
    }
}

/// PG prints a whole-numbered cost / rows without a decimal point.
fn render_attr_number(v: f64) -> alloc::string::String {
    // no_std: `f64::fract` lives in std, so compare against the truncation.
    let whole = v as i64;
    if v.abs() < 1e15 && (whole as f64) == v {
        alloc::format!("{whole}")
    } else {
        alloc::format!("{v}")
    }
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
    /// v7.39 (round 322, V46) — `IMMUTABLE` / `STRICT` / `PARALLEL SAFE` /
    /// `SECURITY DEFINER` / `LEAKPROOF` / `COST` / `ROWS`. PG accepts them
    /// on either side of the body; before this they were a parse error, so
    /// PG's own `pg_dump` output would not restore.
    pub attrs: FunctionAttrs,
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
    /// v7.39 (read01 round 66) — `RETURN NEXT <expr>;`: append one row to the
    /// set a SETOF function is building, and KEEP GOING. Not a return.
    ReturnNext(Expr),
    /// v7.39 (read01 round 66) — `RETURN QUERY <select>;`: append every row the
    /// query yields, and keep going. It used to desugar to a side-effect
    /// statement whose result was DISCARDED — in a SETOF function that is the
    /// whole answer thrown away.
    ReturnQuery(Box<SelectStatement>),
    /// v7.39 (read01 round 68) — `RETURN QUERY EXECUTE <sql expr>`: the dynamic
    /// twin. Its rows go to the set too; it used to run and discard them.
    ReturnQueryExecute { sql: Expr },
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
    /// Name of the function to invoke. The function must exist at
    /// CREATE TRIGGER time — PG18-measured (round 753): PG refuses a
    /// forward reference (`function no_such_fn() does not exist`), so
    /// requiring it IS the PG behaviour (the old note claimed the
    /// opposite).
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
    /// v7.39 (round 138) — `WHEN ( condition )` row-level filter: the row
    /// trigger fires only when the condition (over NEW / OLD) is true. `None`
    /// = no WHEN (fire unconditionally). Not allowed on INSTEAD OF triggers.
    pub when_condition: Option<Expr>,
}

/// v7.39 (round 139) — `CREATE RULE` query-rewrite rule AST node.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRuleStatement {
    pub name: String,
    pub or_replace: bool,
    /// Event keyword, uppercased: `INSERT` / `UPDATE` / `DELETE` / `SELECT`.
    pub event: String,
    pub table: String,
    /// `true` = `DO INSTEAD` (replace the operation), `false` = `DO ALSO`
    /// (run alongside; PG's default when neither keyword is written).
    pub instead: bool,
    /// Optional `WHERE ( condition )` — the rule applies only when it holds.
    pub when_condition: Option<Expr>,
    /// The `DO` commands (over NEW / OLD). Empty = `NOTHING`.
    pub commands: Vec<Statement>,
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

/// v7.39 (round 537) — a `CREATE INDEX` key column's ordering clause.
///
/// SPG's index does not scan in a direction, but `indexdef` reproduces
/// the DDL and dropping this made `(a DESC NULLS LAST)` read back as
/// `(a)`. `nulls_first` is `None` when the statement did not say, in
/// which case PG's default applies — LAST for ascending, FIRST for
/// descending, and neither is rendered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexColumnOrder {
    pub descending: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexStatement {
    pub name: String,
    /// `CREATE INDEX CONCURRENTLY`. SPG builds indexes synchronously
    /// either way, so this changes nothing about how the index is made
    /// — it is carried because PG refuses the CONCURRENTLY form inside
    /// a transaction block and accepts the plain one, and the engine
    /// cannot tell them apart without it.
    pub concurrently: bool,
    /// v7.39 (round 537) — the leading key column's ordering clause,
    /// which is the column SPG indexes.
    pub key_order: IndexColumnOrder,
    /// v7.39 (round 538) — an explicit `COLLATE` on that key, as
    /// written. SPG orders text by bytes, so honouring it changes
    /// nothing; PG prints it, because an explicitly named collation and
    /// the one a column inherits are different objects.
    pub key_collation: Option<String>,
    pub table: String,
    pub column: String,
    /// v7.39 (read01 round 52) — `CREATE UNIQUE INDEX … NULLS NOT DISTINCT`
    /// (PG 15+). Default (`false`) is the SQL-standard NULLS DISTINCT, where
    /// any NULL in the key exempts the row from the uniqueness check.
    pub nulls_not_distinct: bool,
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
    /// v7.39.11 — each extra column's `ASC` / `DESC` / `NULLS FIRST`
    /// / `NULLS LAST`, positionally aligned with `extra_columns`. The
    /// parser used to discard these, so a composite index's direction
    /// survived only on the leading column and `pg_get_indexdef`
    /// rendered `(a, b DESC)` back as `(a, b)`.
    pub extra_orders: Vec<IndexColumnOrder>,
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
    /// r1038 — the access method as WRITTEN, lower-cased; `None` when
    /// there was no `USING` clause.
    ///
    /// `method` cannot answer this: `gist` / `spgist` / `hash` all become
    /// `IndexMethod::BTree` so PG schemas naming an AM SPG has no
    /// implementation for still load. That degradation is deliberate, but
    /// it loses the name — and the operator-class check needs it, both to
    /// look the class up under the AM the user actually named and to say
    /// which AM it was missing from, the way PG's message does.
    pub method_name: Option<String>,
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

/// v7.39 (round 531) — `LIKE <table> [ {INCLUDING|EXCLUDING} <opt> ]*`
/// inside a CREATE TABLE column list.
///
/// The source table's shape can only be read from the catalog, so the
/// parser records the clause and the engine expands it. `at` is how many
/// explicit columns preceded it: PG keeps the written order, so
/// `CREATE TABLE k (x int, LIKE t)` puts `x` first.
#[derive(Debug, Clone, PartialEq)]
pub struct LikeSpec {
    pub source: String,
    pub at: usize,
    pub options: LikeOptions,
    /// v7.40.0 — MySQL's `CREATE TABLE b LIKE a` keeps the source's
    /// index names (`PRIMARY`, `ks`); PostgreSQL's
    /// `CREATE TABLE b (LIKE a INCLUDING ALL)` renames the copies after
    /// the new table (`lb_pkey`, `lb_s_idx`). Both measured. The
    /// spelling says which engine's rule applies.
    pub keep_index_names: bool,
}

/// Which properties `LIKE` carries over. A bare `LIKE` copies names,
/// types and NOT NULL and nothing else — measured on PG18, where a
/// copied generated column becomes a plain one and a copied identity
/// column loses its identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LikeOptions {
    pub defaults: bool,
    pub constraints: bool,
    pub identity: bool,
    pub generated: bool,
    pub indexes: bool,
    pub comments: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStatement {
    /// v7.39 (round 436) — `CREATE TEMPORARY TABLE`. The table lives in the
    /// creating session's own namespace: it shadows a permanent table of the
    /// same name, other sessions never see it, and it is dropped when the
    /// session ends. A `bool` here lands in the struct's existing padding.
    pub temporary: bool,
    pub name: String,
    /// v7.39 — the `ENGINE=` a MySQL dump names. Consumed and discarded
    /// before, so `ENGINE=NONSUCH` built a table where MySQL 9.7.2
    /// answers `ERROR 1286`, and `sql_mode` claimed
    /// `NO_ENGINE_SUBSTITUTION` while doing it.
    pub engine: Option<String>,
    /// v7.40.0 — the `AUTO_INCREMENT=N` table option: the next value
    /// the table hands out. It was consumed and dropped, so the first
    /// row of a table declared `AUTO_INCREMENT=100` got 1 where MySQL
    /// 9.7.2 gives it 100 — and `SHOW CREATE TABLE`, which reproduces
    /// the option from the counter, round-tripped a different number.
    pub auto_increment: Option<i64>,
    pub columns: Vec<ColumnDef>,
    /// v7.39 (round 531) — the `LIKE` clauses in the column list, in
    /// the order written. Empty for a table that has none.
    pub like_specs: Vec<LikeSpec>,
    /// v7.39 (round 645) — `CREATE TABLE c (…) INHERITS (p1, p2)`.
    /// Empty for a table that inherits from nothing. Order matters:
    /// the child takes each parent's columns in this order before its
    /// own, and a parent's position here is its `pg_inherits.inhseqno`.
    pub inherits: Vec<String>,
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
        /// v7.39 (round 711) — `[NOT] DEFERRABLE [INITIALLY DEFERRED]`.
        /// Round 621 consumed the clauses; these carry them.
        deferrable: bool,
        initially_deferred: bool,
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
        /// v7.39 (round 711) — see PrimaryKey.
        deferrable: bool,
        initially_deferred: bool,
        /// v7.40.0 — MySQL's per-column index prefix on a
        /// `UNIQUE KEY k (b(4))`, aligned with `columns`. Unlike a
        /// plain KEY's, this one CHANGES what the constraint accepts:
        /// MySQL rejects two rows sharing the first four characters.
        /// Empty for every PostgreSQL spelling.
        prefix_lengths: Vec<Option<u32>>,
    },
    /// v7.13.0 — `CHECK (<expr>)` table-level constraint
    /// (mailrs round-5 G3). Column-level inline CHECKs fold into
    /// this same variant at parse time. Engine evaluates the
    /// predicate against each INSERT/UPDATE candidate row; a
    /// false / NULL result rejects the mutation.
    /// v7.39 (round 652) — `not_valid` carries the `NOT VALID` suffix.
    /// PG adds such a constraint without scanning the existing rows: new
    /// rows are checked, the ones already there are grandfathered in, and
    /// `pg_constraint.convalidated` reads `f` until `VALIDATE CONSTRAINT`
    /// scans and flips it. pg_dump emits the suffix for exactly those, so
    /// validating them on restore would refuse a dump PG itself produced.
    Check {
        name: Option<String>,
        expr: Expr,
        not_valid: bool,
    },
    /// v7.39 (round 210) — `EXCLUDE [USING <method>] (<col> WITH <op>
    /// [, …])`: no two rows may satisfy `(r.c1 op1 s.c1) AND …` for
    /// every element (the booking/scheduling non-overlap constraint,
    /// `EXCLUDE USING gist (during WITH &&)`). `method` is the index
    /// AM name (gist/spgist/btree — informational in Phase 0; the O(n)
    /// enforcement doesn't build the index yet). Each element pairs a
    /// column name with an operator spelling (`&&`, `=`, `@>`, …).
    Exclude {
        name: Option<String>,
        method: Option<String>,
        elements: Vec<(String, String)>,
    },
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
        /// v7.40.0 — MySQL's per-column index prefix, `KEY k (b(4))`,
        /// positionally aligned with `columns`. `None` for a column
        /// written without one, which is every PostgreSQL index key.
        ///
        /// It was skipped by the parser and dropped, so the declaration
        /// was accepted and the index built over the whole column with
        /// nothing recording that a prefix had been asked for —
        /// `SHOW INDEX` then reported `Sub_part` NULL and
        /// `SHOW CREATE TABLE` printed `(b)` where MySQL 9.7.2 prints
        /// `(b(4))`.
        prefix_lengths: Vec<Option<u32>>,
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
    /// v7.38 (read01 P4.19) — `UNIQUE NULLS NOT DISTINCT` (PG 15+) on the
    /// inline column constraint: treat NULL keys as equal so only one NULL
    /// is allowed. Ignored unless `is_unique`. Folded into the table-level
    /// `TableConstraint::Unique { nulls_not_distinct }`.
    pub unique_nulls_not_distinct: bool,
    /// v7.39 (round 711) — `DEFERRABLE [INITIALLY DEFERRED]` written on the
    /// inline PK/UNIQUE column constraint. Consumed since round 621; carried
    /// since this round so the fold into the table-level constraint keeps it.
    pub constraint_deferrable: bool,
    pub constraint_initially_deferred: bool,
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
    /// v7.39 (round 370, M4 P4a) — whether `collation` came from an
    /// explicit `COLLATE <name>` clause rather than the default. Under the
    /// MySQL dialect a text column with NO explicit clause takes the
    /// folding default collation, while an explicit `COLLATE utf8mb4_bin`
    /// stays byte-wise — and both resolve to `Collation::Binary`, so this
    /// flag is the only thing that tells them apart.
    pub collation_explicit: bool,
    /// v7.39 (round 676) — the collation name AS WRITTEN, because
    /// `collation` above cannot carry it: `Collation` is a two-variant
    /// MySQL enum and `from_collation_name` folds `C`, `POSIX`, `en_US` and
    /// `default` all into `Binary`. `pg_attribute.attcollation` needs to
    /// tell them apart.
    pub collation_name: Option<String>,
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
    /// v7.38 (read01) — `GENERATED ALWAYS AS IDENTITY` (as opposed to
    /// `GENERATED BY DEFAULT AS IDENTITY`). Both flavours set
    /// `auto_increment`; this additionally marks the ALWAYS one, whose
    /// explicit INSERT value PG rejects ("cannot insert a non-DEFAULT
    /// value into column …") unless the INSERT carries `OVERRIDING SYSTEM
    /// VALUE`. Only meaningful when the column is also an identity column.
    pub identity_always: bool,
    /// v7.39 (round 386, type-fidelity epic P1) — the declared MySQL narrow
    /// integer width (TINYINT / MEDIUMINT), captured before the type
    /// collapses to SmallInt / Int. The engine copies it to
    /// `ColumnSchema.mysql_int_width` at CREATE TABLE time so the write
    /// path can enforce the real range. None for every other column and
    /// under the PG dialect.
    pub mysql_int_width: Option<MysqlIntWidth>,
    /// v7.39 (round 424, type-fidelity epic) — the declared MySQL
    /// fractional-seconds precision of a temporal column (`DATETIME(3)` is
    /// `Some(3)`; a bare `DATETIME` / `TIME` / `TIMESTAMP` is `Some(0)`,
    /// MySQL's default). The engine copies it to `ColumnSchema.mysql_fsp` at
    /// CREATE TABLE time so the write path can truncate and the render path
    /// can pad. None under the PG dialect, where temporal columns keep full
    /// microseconds.
    pub mysql_fsp: Option<u8>,
    /// v7.39.2 — the column was written `TIMESTAMP` rather than
    /// `DATETIME` in a MySQL session. The engine copies it to
    /// `ColumnSchema.mysql_declared_timestamp` at CREATE TABLE.
    pub mysql_declared_timestamp: bool,
    /// v7.39.3 — a MySQL `FLOAT(m,d)` / `DOUBLE(m,d)` pair, copied to
    /// `ColumnSchema.mysql_float_md` at CREATE TABLE.
    pub mysql_float_md: Option<(u8, u8)>,
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

/// v7.39 (round 386, type-fidelity epic P1) — the declared MySQL narrow
/// integer width for a column whose `ColumnTypeName` is too wide to carry
/// it: `TINYINT` collapses to `SmallInt`, `MEDIUMINT` to `Int`. Mirrors
/// `spg_storage::MysqlIntWidth`; the engine bridges the two at CREATE
/// TABLE time. Only recorded under the MySQL dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MysqlIntWidth {
    Tiny,
    Small,
    Medium,
    Int,
    /// v7.39 (round 471, epic P4b) — `BIGINT UNSIGNED`.
    Big,
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
    /// v7.38 (read01, T29) — `MATCH {SIMPLE | FULL}`. Defaults to `Simple`.
    pub match_type: MatchType,
    /// v7.39 (round 288) — `[NOT] DEFERRABLE`. Parsed since v7.17 and
    /// dropped on the floor, so a constraint declared DEFERRABLE was
    /// enforced immediately and a circular-FK migration could not load.
    pub deferrable: bool,
    /// `INITIALLY DEFERRED` — the check moves to COMMIT unless
    /// `SET CONSTRAINTS … IMMEDIATE` pulls it forward.
    pub initially_deferred: bool,
}

/// v7.38 (read01, T29) — FK `MATCH` type. SIMPLE (default) skips the check when
/// ANY referencing column is NULL; FULL requires all-or-none NULL (a mixed-NULL
/// key errors). PARTIAL is parse-rejected (PG does not implement it either).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchType {
    #[default]
    Simple,
    Full,
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
    /// v7.39 (round 291) — PG's `name`, the identifier type its
    /// catalogs use. `CREATE TABLE t (a name)` is legal SQL that SPG
    /// answered `type "name" does not exist` to.
    Name,
    /// v7.39 (round 640) — PG's transaction-id types. `xid` is the
    /// 32-bit wrapping counter the row header carries; `xid8` is the
    /// 64-bit monotonic one. `CREATE TABLE t (a xid)` is legal SQL that
    /// SPG answered `type "xid" does not exist` to.
    Xid,
    Xid8,
    /// v7.39 (round 667) — `OID`. `XID` was already a column type here
    /// and `OID` was not, so `CREATE TABLE t(o OID)` answered
    /// `type "oid" does not exist` while `t(x XID)` built fine.
    Oid,
    SmallInt,
    Int,
    BigInt,
    Float,
    /// v7.39 (round 269) — `REAL` / `FLOAT4` / `FLOAT(1..24)`: 32-bit
    /// IEEE. It used to map to [`Self::Float`] on the theory that a
    /// wider float is harmless, but the width is observable: a `real`
    /// column holding 0.1 stored the f64 0.1, so `r = 0.1::real`
    /// answered false where PG answers true.
    Real,
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
    /// v7.39 (round 271) — scale widened to u16 alongside the value's.
    /// v7.39 (round 272) — precision too: PG's runs to 1000.
    /// v7.39 (round 273) — the DECLARED scale is signed (-1000..=1000);
    /// a negative one rounds to tens / hundreds. A VALUE's display scale
    /// stays unsigned.
    Numeric(u16, i16),
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
    /// v7.40.0 `OID[]` — single-dimension oid array. PG wire OID
    /// 1028.
    ///
    /// `DataType::OidArray` and its value, codec tag, wire encoding
    /// and every naming surface have existed since v7.39 (round 694);
    /// what was missing was only the DDL spelling, so
    /// `CREATE TABLE t (c oid[])` answered `Oid[] not yet supported`
    /// while PostgreSQL 18.6 accepts it. Capability present, routing
    /// absent — the same shape as this repository's other hand-kept
    /// lists.
    OidArray,
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
    /// v7.39 (read01 round 75) — `bool[][]`.
    BoolArray2D,
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
    /// v7.40.0 — five array spellings PG 18.6 accepts at
    /// `CREATE TABLE` and SPG refused at the type name. The
    /// element types were all present; only the `[]` step was.
    RealArray,
    TimeArray,
    TimeTzArray,
    InetArray,
    XmlArray,
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
    /// v7.39 (round 281) — `BIT(n)`; `0` = no typmod (PG: `bit(1)`).
    Bit(u32),
    /// v7.39 (round 281) — `BIT VARYING(n)`; `0` = unbounded.
    BitVarying(u32),
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
            Self::Real => f.write_str("REAL"),
            Self::Text => f.write_str("TEXT"),
            Self::Name => f.write_str("name"),
            Self::Xid => f.write_str("xid"),
            Self::Xid8 => f.write_str("xid8"),
            Self::Oid => f.write_str("oid"),
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
            Self::OidArray => f.write_str("oid[]"),
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
            Self::RealArray => f.write_str("REAL[]"),
            Self::TimeArray => f.write_str("TIME[]"),
            Self::TimeTzArray => f.write_str("TIMETZ[]"),
            Self::InetArray => f.write_str("INET[]"),
            Self::XmlArray => f.write_str("XML[]"),
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
            Self::Bit(0) => f.write_str("BIT"),
            Self::Bit(n) => write!(f, "BIT({n})"),
            Self::BitVarying(0) => f.write_str("VARBIT"),
            Self::BitVarying(n) => write!(f, "VARBIT({n})"),
            Self::Xml => f.write_str("XML"),
            Self::Char1 => f.write_str("\"char\""),
            Self::MoneyArray => f.write_str("MONEY[]"),
            Self::IntArray2D => f.write_str("INT[][]"),
            Self::BigIntArray2D => f.write_str("BIGINT[][]"),
            Self::TextArray2D => f.write_str("TEXT[][]"),
            Self::BoolArray2D => f.write_str("BOOL[][]"),
        }
    }
}

/// `UPDATE <table> SET col = expr [, ...] [WHERE cond]`. v4.4 — the
/// engine evaluates `expr` per matched row in the table's row order
/// and rewrites cells in place. Indexed columns are dropped + re-
/// inserted into the affected B-tree on each row change.
/// v7.39 (round 413) — the boxed payload for MySQL's `ORDER BY [LIMIT]`
/// tail on a DML statement. Boxed off the statement struct so the PG-only
/// common path stays at its pre-r413 size (see the round-305 nesting-stack
/// lesson). v7.39 (round 431+1) — DELETE carries the identical clause with
/// the identical meaning, so both share this one payload rather than each
/// growing its own.
#[derive(Debug, Clone, PartialEq)]
pub struct DmlOrderLimit {
    pub order_by: Vec<OrderBy>,
    pub limit: Option<u32>,
}

/// v7.39 (round 533) — what `UPDATE … FROM src WHERE cond` was lowered
/// FROM, kept so the engine can finish the job.
///
/// The parser rewrites the statement onto correlated subqueries, and it
/// can only classify a QUALIFIED leaf: deciding whether an unqualified
/// name belongs to the target or to a source needs their column lists,
/// which parse time does not have. Carrying the clause lets the engine
/// — which has the catalog — resolve the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateFromSources {
    pub from: FromClause,
    pub sub_where: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatement {
    /// v7.37.43-T4.4 — leading `WITH cte AS (…)` clauses on a top-
    /// level UPDATE. Empty for a plain UPDATE.
    pub ctes: Vec<Cte>,
    pub table: String,
    /// v7.39 (round 646) — `UPDATE ONLY t` / `DELETE FROM ONLY t`: apply
    /// to `t`'s own rows and not to anything that descends from it.
    ///
    /// Round 644 taught the FROM clause the keyword and left DML behind
    /// because it needed a field here, and this struct carries a warning
    /// that round 413 measured widening it in place overflowing the
    /// parser's nesting stack. That warning was about `from_sources`, a
    /// struct wide enough to need boxing; a `bool` lands in the padding
    /// already present — same as `CreateTableStatement::temporary`.
    ///
    /// It also earns its keep beyond the spelling: the inheritance
    /// fan-out needs a way to say "the parent's own rows" as a
    /// statement, or running one on the parent recurses forever.
    pub only: bool,
    /// v7.39 (round 241) — `UPDATE t [AS] alias SET …`: the name the
    /// statement's expressions refer to the target row by. PG allows the
    /// bare spelling here (unlike INSERT, which requires AS).
    pub alias: Option<String>,
    pub assignments: Vec<(String, Expr)>,
    /// v7.39 (round 533) — boxed: round 413 measured that widening this
    /// struct in place overflows the parser's nesting stack.
    pub from_sources: Option<alloc::boxed::Box<UpdateFromSources>>,
    pub where_: Option<Expr>,
    /// v7.39 (round 413) — MySQL's `UPDATE … [ORDER BY … [LIMIT n]]`:
    /// mutate the first `limit` rows in the given order. PG has no such
    /// clause; the parser accepts it only under the MySQL dialect. Boxed
    /// so a PG UPDATE (the common case) grows this struct by ONE pointer,
    /// not `Vec<OrderBy> + Option<u32>` — a naked add tipped the parser's
    /// 512 KiB nesting stack under a full workspace test (round 305 kin).
    pub order_limit: Option<alloc::boxed::Box<DmlOrderLimit>>,
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
    /// v7.39 (round 646) — `DELETE FROM ONLY t`, the sibling of `UpdateStatement::only`: apply
    /// to `t`'s own rows and not to anything that descends from it.
    ///
    /// Round 644 taught the FROM clause the keyword and left DML behind
    /// because it needed a field here, and this struct carries a warning
    /// that round 413 measured widening it in place overflowing the
    /// parser's nesting stack. That warning was about `from_sources`, a
    /// struct wide enough to need boxing; a `bool` lands in the padding
    /// already present — same as `CreateTableStatement::temporary`.
    ///
    /// It also earns its keep beyond the spelling: the inheritance
    /// fan-out needs a way to say "the parent's own rows" as a
    /// statement, or running one on the parent recurses forever.
    pub only: bool,
    /// v7.39 (round 241) — `DELETE FROM t [AS] alias USING …`: the name
    /// the WHERE / RETURNING expressions refer to the target row by.
    pub alias: Option<String>,
    pub where_: Option<Expr>,
    /// v7.39 (round 432) — MySQL's `DELETE … [ORDER BY … [LIMIT n]]`, the
    /// batched-cleanup idiom. Same clause and same meaning as the UPDATE
    /// form (round 413), so it shares that payload — and it is boxed for
    /// the same reason: a naked `Vec<OrderBy> + Option<u32>` on a DML
    /// statement tipped the parser's 512 KiB nesting stack.
    pub order_limit: Option<alloc::boxed::Box<DmlOrderLimit>>,
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
    /// v7.39 (read01 round 149) — leading `WITH <cte> [, …]` (PG 15 allows
    /// a WITH clause on MERGE; `WITH RECURSIVE` is rejected at parse, as
    /// in PG). Each CTE materialises before the merge runs and its alias
    /// resolves as a source relation.
    pub ctes: Vec<Cte>,
    pub target: String,
    pub target_alias: Option<String>,
    pub source: String,
    pub source_alias: Option<String>,
    /// v7.37 D.44 — `USING (SELECT …) alias` subquery source. When present,
    /// the engine materialises this SELECT for the source rows and `source`
    /// is empty; the alias (required by PG for a subquery source) is in
    /// `source_alias`. `None` = plain `USING <table>` (source names a table).
    pub source_select: Option<Box<SelectStatement>>,
    /// v7.39 (round 768, F31-D5) — `USING (VALUES …) s(id, v)`: the
    /// positional column-alias list after the source alias. Empty when
    /// the statement carries none; the engine renames the materialised
    /// source columns positionally (PG's rule).
    pub source_column_aliases: Vec<String>,
    pub on: Expr,
    pub clauses: Vec<MergeWhenClause>,
    /// v7.39 (read01 round 130) — PG17+ `MERGE … RETURNING <projection>`.
    /// The projection may use `merge_action()`, `OLD.*`/`NEW.*`, and the
    /// target/source aliases. `None` = no RETURNING (the common form).
    pub returning: Option<Vec<SelectItem>>,
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
    /// `WHEN NOT MATCHED [BY TARGET]` — a source row with no matching
    /// target row (the classic insert branch).
    NotMatched,
    /// v7.39 (round 146, PG17) — `WHEN NOT MATCHED BY SOURCE`: a TARGET
    /// row no source row matches. Actions are UPDATE / DELETE / DO
    /// NOTHING only (INSERT is a syntax error, as in PG).
    NotMatchedBySource,
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
    /// v7.39 (round 240) — `INSERT INTO t AS alias`: the alias the ON
    /// CONFLICT DO UPDATE expressions (and RETURNING) refer to the target
    /// row by. PG requires the AS keyword in this position.
    pub alias: Option<String>,
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
    /// v7.38 (read01) — `OVERRIDING { SYSTEM | USER } VALUE` clause
    /// between the column list and VALUES. Governs how explicitly-supplied
    /// values interact with `GENERATED … AS IDENTITY` columns:
    ///   * `None` — default. A `GENERATED ALWAYS` identity column rejects
    ///     an explicit non-DEFAULT value; a `BY DEFAULT` one accepts it.
    ///   * `System` — override the ALWAYS restriction: the explicit value
    ///     is used verbatim, as for a `BY DEFAULT` column.
    ///   * `User` — ignore any explicit value on a `BY DEFAULT` identity
    ///     column and generate from the sequence instead (no effect on
    ///     non-identity columns).
    pub overriding: Overriding,
    /// v7.39 (round 434) — the statement was spelled `INSERT IGNORE`.
    /// Round 406 lowered that to `ON CONFLICT DO NOTHING`, which covers the
    /// key-conflict half of MySQL's IGNORE. The other half is that IGNORE
    /// also downgrades per-VALUE errors to coercions (out-of-range clamps,
    /// over-long strings truncate, a non-numeric string becomes 0, a NULL
    /// into a NOT NULL column becomes the type's default), and the engine
    /// cannot recover that intent from the conflict clause alone. A plain
    /// `bool` lands in this struct's existing padding, so the AST does not
    /// grow — measured, per the round-305 / 413 nesting-stack lesson.
    pub mysql_ignore: bool,
}

/// v7.38 (read01) — `OVERRIDING { SYSTEM | USER } VALUE` on an INSERT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overriding {
    /// No `OVERRIDING` clause.
    #[default]
    None,
    /// `OVERRIDING SYSTEM VALUE`.
    System,
    /// `OVERRIDING USER VALUE`.
    User,
}

/// v7.9.7 — INSERT upsert clause: `ON CONFLICT (target) DO action`.
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflictClause {
    /// Local columns that identify the conflict (must match a
    /// UNIQUE / PRIMARY KEY index on the target table). Empty
    /// list means the user wrote `ON CONFLICT DO …` without a
    /// target — the engine arbitrates on every unique constraint
    /// (round 240).
    pub target_columns: Vec<String>,
    /// v7.39 (round 240) — the index predicate after the target list
    /// (`ON CONFLICT (col) WHERE pred DO …`). PG uses it to infer a
    /// PARTIAL unique index; SPG's conflict arbiters are full indexes,
    /// which satisfy any predicate, so it is parsed and carried but not
    /// consulted (recorded residual: partial-unique-index arbiters).
    pub index_where: Option<Expr>,
    /// v7.37.17 (17.6 siblings) — `ON CONFLICT ON CONSTRAINT
    /// <name>`: the pg_dump conflict-target form. The engine
    /// resolves the name to the constraint's columns.
    pub constraint_name: Option<String>,
    /// v7.39 (round 240) — true when this clause was LOWERED from MySQL's
    /// `ON DUPLICATE KEY UPDATE` / `REPLACE INTO`, whose bare-target DO
    /// UPDATE is legal (MySQL watches every unique key); PG's own bare
    /// `ON CONFLICT DO UPDATE` is refused (42601).
    pub mysql_lowered: bool,
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

/// v7.39 (round 293, E3 Phase 1) — a row-locking clause.
///
/// `spg-sql` cannot depend on `spg-engine`, so the strengths and
/// policies are spelled again here and mapped at the engine boundary.
/// v7.39 — the modes `BEGIN` / `START TRANSACTION` / `SET TRANSACTION`
/// / `SET SESSION CHARACTERISTICS` accept. `read_only` used to be parsed
/// and dropped on the floor, so `BEGIN READ ONLY` opened an ordinary
/// read-write transaction and every write in it was accepted.
///
/// `None` on either field means the statement did not name that mode, so
/// the session default applies — which is not the same as naming the
/// default explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransactionModes {
    pub isolation: Option<IsolationLevel>,
    /// `Some(true)` = READ ONLY, `Some(false)` = READ WRITE.
    pub read_only: Option<bool>,
}

/// v7.39 — what a read-only transaction refuses, and what PG calls it.
///
/// SPG did not enforce read-only transactions at all: `BEGIN READ ONLY;
/// INSERT …` answered `INSERT 0 1` and committed, and
/// `default_transaction_read_only = on` changed nothing. Both GUCs were
/// in the inventory, so a session could set one, read it back, and be
/// told it held a guarantee nothing was enforcing. Applications open
/// read-only transactions as a SAFETY measure — a reporting connection,
/// a read-only leg in a pool, a "this path must not write" discipline —
/// so accepting the writes is the worst possible answer.
///
/// `Some(tag)` means refuse with PG's message, `cannot execute {tag} in
/// a read-only transaction` (SQLSTATE 25006). Every tag below was read
/// back from PostgreSQL 18.6 by running the statement inside
/// `BEGIN READ ONLY`, one statement per transaction so no error could be
/// attributed to the wrong line.
///
/// Several answers were not what one would guess, which is why they were
/// measured rather than reasoned:
///
///   * `CREATE TEMP TABLE` is REFUSED, tagged `CREATE TABLE`.
///   * `NOTIFY`, `LISTEN` and `REINDEX` are ALLOWED.
///   * `PREPARE` of an INSERT is ALLOWED — only the EXECUTE writes.
///   * `UPDATE … WHERE false`, which changes nothing, is still REFUSED:
///     the verb decides, not the row count.
///   * `GRANT`, `COMMENT ON` and `SELECT … FOR SHARE` are all REFUSED.
///
/// The match is exhaustive on purpose. A new statement cannot be added
/// without deciding here whether it writes, which is the failure this
/// repository keeps meeting: one member of a family gets handled and its
/// siblings quietly do not.
impl Statement {
    #[must_use]
    pub fn read_only_violation_tag(&self) -> Option<&'static str> {
        match self {
            // v7.39.9 — MySQL's RENAME TABLE is DDL, refused read-only
            // for the same reason ALTER TABLE … RENAME TO is.
            Self::RenameTables(_) => Some("RENAME TABLE"),
            // ---- writes rows -------------------------------------------
            Self::Insert { .. } => Some("INSERT"),
            Self::Update { .. } => Some("UPDATE"),
            Self::Delete { .. } => Some("DELETE"),
            Self::Merge { .. } => Some("MERGE"),
            Self::Truncate { .. } => Some("TRUNCATE TABLE"),
            Self::CopyFromFile { .. } => Some("COPY FROM"),

            // A SELECT that takes row locks writes lock state, and PG
            // names the strength it was asked for.
            Self::Select(sel) => sel.locking.as_ref().map(|l| match l.strength {
                LockStrength::Update => "SELECT FOR UPDATE",
                LockStrength::NoKeyUpdate => "SELECT FOR NO KEY UPDATE",
                LockStrength::Share => "SELECT FOR SHARE",
                LockStrength::KeyShare => "SELECT FOR KEY SHARE",
            }),

            // ---- changes the catalog -----------------------------------
            Self::CreateTable { .. } => Some("CREATE TABLE"),
            Self::DropTable { .. } => Some("DROP TABLE"),
            Self::AlterTable { .. } => Some("ALTER TABLE"),
            Self::CreateIndex { .. } => Some("CREATE INDEX"),
            Self::DropIndex { .. } => Some("DROP INDEX"),
            Self::AlterIndex { .. } => Some("ALTER INDEX"),
            Self::CreateView { .. } => Some("CREATE VIEW"),
            Self::DropView { .. } => Some("DROP VIEW"),
            Self::CreateMaterializedView { .. } => Some("CREATE MATERIALIZED VIEW"),
            Self::RefreshMaterializedView { .. } => Some("REFRESH MATERIALIZED VIEW"),
            Self::DropMaterializedView { .. } => Some("DROP MATERIALIZED VIEW"),
            Self::CreateSequence { .. } => Some("CREATE SEQUENCE"),
            Self::AlterSequence { .. } => Some("ALTER SEQUENCE"),
            Self::DropSequence { .. } => Some("DROP SEQUENCE"),
            Self::CreateType { .. } => Some("CREATE TYPE"),
            Self::DropType { .. } => Some("DROP TYPE"),
            Self::AlterTypeAddValue { .. } | Self::AlterTypeRenameValue { .. } => {
                Some("ALTER TYPE")
            }
            Self::CreateDomain { .. } => Some("CREATE DOMAIN"),
            Self::AlterDomain { .. } => Some("ALTER DOMAIN"),
            Self::DropDomain { .. } => Some("DROP DOMAIN"),
            Self::CreateSchema { .. } => Some("CREATE SCHEMA"),
            Self::DropSchema { .. } => Some("DROP SCHEMA"),
            Self::CreateFunction { .. } => Some("CREATE FUNCTION"),
            Self::DropFunction { .. } => Some("DROP FUNCTION"),
            Self::CreateTrigger { .. } => Some("CREATE TRIGGER"),
            Self::DropTrigger { .. } => Some("DROP TRIGGER"),
            Self::CreateRule { .. } => Some("CREATE RULE"),
            Self::DropRule { .. } => Some("DROP RULE"),
            Self::CreateExtension { .. } => Some("CREATE EXTENSION"),
            Self::CreateStatistics { .. } => Some("CREATE STATISTICS"),
            Self::DropStatistics { .. } => Some("DROP STATISTICS"),
            Self::DropAggregate { .. } => Some("DROP AGGREGATE"),
            Self::CommentOn { .. } => Some("COMMENT"),
            Self::DropDatabase { .. } => Some("DROP DATABASE"),
            Self::CreatePublication { .. } => Some("CREATE PUBLICATION"),
            Self::DropPublication { .. } => Some("DROP PUBLICATION"),
            Self::CreateSubscription { .. } => Some("CREATE SUBSCRIPTION"),
            Self::DropSubscription { .. } => Some("DROP SUBSCRIPTION"),

            // ---- changes roles / permissions ---------------------------
            Self::CreateUser { .. } => Some("CREATE ROLE"),
            Self::DropUser { .. } => Some("DROP ROLE"),
            Self::AlterRolePassword { .. } | Self::SetDbRoleSetting { .. } => Some("ALTER ROLE"),
            Self::Grant { .. } => Some("GRANT"),
            Self::Revoke { .. } => Some("REVOKE"),
            Self::CreatePolicy { .. } => Some("CREATE POLICY"),
            Self::AlterPolicy { .. } => Some("ALTER POLICY"),
            Self::DropPolicy { .. } => Some("DROP POLICY"),
            Self::AlterSystem { .. } => Some("ALTER SYSTEM"),

            // ---- SPG's own writers -------------------------------------
            // Rewrites cold-tier segments on disk. PG has no equivalent to
            // ask, so the test is what it does, not what it is called.
            Self::CompactColdSegments => Some("COMPACT COLD SEGMENTS"),

            // ---- allowed -----------------------------------------------
            // Reads, transaction control, session state, cursors, and the
            // maintenance statements PG itself permits. `REINDEX` really is
            // allowed in a read-only transaction (measured), which is why
            // `Maintain` is here.
            //
            // `Prepare`, `Execute`, `Call` and `DoBlock` are allowed at
            // this level for the reason PG allows them: the write inside
            // is refused when it runs, by this same check. Measured:
            // `PREPARE p AS INSERT …` succeeds; `DO $$ … INSERT … $$`
            // fails with `cannot execute INSERT`.
            Self::Explain { .. }
            | Self::CopyTo { .. }
            | Self::CopyToFile { .. }
            | Self::Analyze { .. }
            | Self::Maintain { .. }
            | Self::Vacuum { .. }
            | Self::Begin { .. }
            | Self::Commit
            | Self::Rollback
            | Self::Savepoint { .. }
            | Self::RollbackToSavepoint { .. }
            | Self::ReleaseSavepoint { .. }
            | Self::PrepareTransaction { .. }
            | Self::SetTransaction { .. }
            | Self::SetConstraints { .. }
            | Self::SetParameter { .. }
            | Self::SetParameterList { .. }
            | Self::SetUserVars { .. }
            | Self::SetRole { .. }
            | Self::ResetParameter { .. }
            | Self::ShowParameter { .. }
            | Self::Discard { .. }
            | Self::Prepare { .. }
            | Self::Execute { .. }
            | Self::Deallocate { .. }
            | Self::Call { .. }
            | Self::DoBlock { .. }
            | Self::DeclareCursor { .. }
            | Self::FetchCursor { .. }
            | Self::MoveCursor { .. }
            | Self::CloseCursor { .. }
            | Self::Listen { .. }
            | Self::Notify { .. }
            | Self::Unlisten { .. }
            | Self::Kill { .. }
            | Self::WaitForWalPosition { .. }
            | Self::ValidateOnly { .. }
            | Self::NoOpPreventedInTransaction { .. }
            | Self::Empty
            | Self::ShowTables
            | Self::ShowDatabases
            | Self::UseDatabase(_)
            | Self::ShowCreateTable { .. }
            | Self::ShowIndexes { .. }
            | Self::ShowStatus
            | Self::ShowVariables
            | Self::ShowVariablesLike { .. }
            | Self::ShowProcesslist
            | Self::ShowColumns { .. }
            | Self::ShowUsers
            | Self::ShowPublications
            | Self::ShowSubscriptions => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockingClause {
    pub strength: LockStrength,
    /// `FOR UPDATE OF t1, t2` — empty means every relation in the FROM.
    pub of_tables: Vec<String>,
    pub policy: LockWait,
}

/// PG's four tuple-lock strengths, weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStrength {
    KeyShare,
    Share,
    NoKeyUpdate,
    Update,
}

/// What to do when the row is already locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockWait {
    /// Block until it is free — PG's default.
    #[default]
    Wait,
    /// `NOWAIT` — fail the statement with 55P03.
    NoWait,
    /// `SKIP LOCKED` — leave the row out of the result.
    SkipLocked,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SelectStatement {
    /// v7.39 (round 293, E3 Phase 1) — `FOR UPDATE` and friends. The
    /// clause was parsed and DISCARDED since v7.17, so SPG accepted the
    /// whole syntax and locked nothing: two workers running the classic
    /// `SKIP LOCKED` queue take both took the same row.
    /// v7.39 (round 305) — boxed. A locking clause appears on a
    /// vanishing fraction of SELECTs, but an inline `Option<LockingClause>`
    /// cost every `SelectStatement` 32 bytes, and this struct sits in
    /// recursive evaluation frames where the engine already runs close to
    /// its stack budget (a 512 KB depth guard is the canary).
    pub locking: Option<alloc::boxed::Box<LockingClause>>,
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
    /// v7.39 (round 705) — the key expressions of WINDOW-clause definitions
    /// that NOTHING referenced. PG analyses every definition whether
    /// referenced or not, so `WINDOW w AS (ORDER BY nosuch)` fails there
    /// and silently succeeded here — the referenced ones get their columns
    /// resolved through the WindowFunction nodes they were inlined into,
    /// and the unreferenced ones used to be dropped at parse, unexamined.
    /// The engine resolves these with a LIMIT-0 probe of the same FROM.
    ///
    /// Not part of `Display`: an unreferenced definition has no effect on
    /// the result, so a deparsed body (a stored view) omits it.
    pub window_check_exprs: Vec<Expr>,
}

impl Expr {
    /// v7.39 (round 305, V23) — hand every `SelectStatement` nested
    /// directly inside this expression to `f`. `f` receives each nested
    /// statement once; descending further (into that statement's own
    /// clauses) is the caller's job, which keeps this walk finite and
    /// lets the caller order the recursion.
    ///
    /// The match is deliberately **wildcard-free**: a new `Expr` variant
    /// does not compile until it says whether it can carry a subquery.
    /// The row-count resolution pass is built on this, and a shape it
    /// silently failed to visit would leave a `LimitExpr::Expr` behind —
    /// which every row-count reader would take as "no limit", i.e. the
    /// whole table. Compile-time exhaustiveness is what rules that out.
    /// Iterative on purpose. Expression trees here get deep (long
    /// boolean chains, big IN lists), and this walk is on the path of
    /// every statement; recursing would add a frame per node to a stack
    /// budget the engine already runs close to — a depth guard that runs
    /// on a deliberately small stack caught exactly that. Depth costs
    /// heap here instead.
    pub fn for_each_subquery_mut<E>(
        &mut self,
        f: &mut impl FnMut(&mut SelectStatement) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut stack: Vec<&mut Self> = alloc::vec![self];
        while let Some(e) = stack.pop() {
            match e {
                Self::Literal(_) | Self::Column(_) | Self::Placeholder(_) => {}
                Self::NamedArg { expr, .. }
                | Self::Collate { expr, .. }
                | Self::Variadic(expr)
                | Self::Unary { expr, .. }
                | Self::Cast { expr, .. }
                | Self::FieldAccess { base: expr, .. }
                | Self::IsNull { expr, .. }
                | Self::BoolTest { expr, .. }
                | Self::Extract { source: expr, .. } => stack.push(expr),
                Self::Binary { lhs, rhs, .. } => {
                    stack.push(lhs);
                    stack.push(rhs);
                }
                Self::Like { expr, pattern, .. } => {
                    stack.push(expr);
                    stack.push(pattern);
                }
                Self::ArraySubscript { target, index } => {
                    stack.push(target);
                    stack.push(index);
                }
                Self::ArraySlice { target, lo, hi } => {
                    stack.push(target);
                    stack.extend(lo.iter_mut().chain(hi.iter_mut()).map(|b| &mut **b));
                }
                Self::AnyAll { expr, array, .. } => {
                    stack.push(expr);
                    stack.push(array);
                }
                Self::FunctionCall { args, .. } | Self::Array(args) => {
                    stack.extend(args.iter_mut());
                }
                Self::AggregateOrdered {
                    call,
                    order_by,
                    filter,
                    ..
                } => {
                    stack.push(call);
                    stack.extend(order_by.iter_mut().map(|o| &mut o.expr));
                    stack.extend(filter.iter_mut().map(|b| &mut **b));
                }
                Self::WindowFunction {
                    args,
                    partition_by,
                    order_by,
                    filter,
                    ..
                } => {
                    // `frame` bounds hold folded numbers / interval
                    // parts, never expressions — nothing to visit there.
                    stack.extend(args.iter_mut().chain(partition_by.iter_mut()));
                    stack.extend(order_by.iter_mut().map(|(e, _, _)| e));
                    stack.extend(filter.iter_mut().map(|b| &mut **b));
                }
                Self::InList { expr, list, .. } => {
                    stack.push(expr);
                    stack.extend(list.iter_mut());
                }
                Self::Case {
                    operand,
                    branches,
                    else_branch,
                } => {
                    stack.extend(
                        operand
                            .iter_mut()
                            .chain(else_branch.iter_mut())
                            .map(|b| &mut **b),
                    );
                    for (when, then) in branches.iter_mut() {
                        stack.push(when);
                        stack.push(then);
                    }
                }
                Self::ScalarSubquery(s) | Self::Exists { subquery: s, .. } => f(s)?,
                Self::InSubquery { expr, subquery, .. } => {
                    stack.push(expr);
                    f(subquery)?;
                }
                Self::RowInSubquery { row, subquery, .. }
                | Self::RowCmpSubquery { row, subquery, .. } => {
                    stack.extend(row.iter_mut());
                    f(subquery)?;
                }
            }
        }
        Ok(())
    }

    /// The shared twin of [`Self::for_each_subquery_mut`], for analysis
    /// that reads a statement rather than rewriting it.
    ///
    /// Same wildcard-free match, same iterative walk. Rust cannot write
    /// one body generic over `&`/`&mut`, so the two must be edited
    /// together; exhaustiveness is what makes that a compile error
    /// rather than a silent gap in one of them.
    ///
    /// # Errors
    /// Whatever `f` returns.
    pub fn for_each_subquery<'a, E>(
        &'a self,
        f: &mut impl FnMut(&'a SelectStatement) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut stack: Vec<&'a Self> = alloc::vec![self];
        while let Some(e) = stack.pop() {
            match e {
                Self::Literal(_) | Self::Column(_) | Self::Placeholder(_) => {}
                Self::NamedArg { expr, .. }
                | Self::Collate { expr, .. }
                | Self::Variadic(expr)
                | Self::Unary { expr, .. }
                | Self::Cast { expr, .. }
                | Self::FieldAccess { base: expr, .. }
                | Self::IsNull { expr, .. }
                | Self::BoolTest { expr, .. }
                | Self::Extract { source: expr, .. } => stack.push(expr),
                Self::Binary { lhs, rhs, .. } => {
                    stack.push(lhs);
                    stack.push(rhs);
                }
                Self::Like { expr, pattern, .. } => {
                    stack.push(expr);
                    stack.push(pattern);
                }
                Self::ArraySubscript { target, index } => {
                    stack.push(target);
                    stack.push(index);
                }
                Self::ArraySlice { target, lo, hi } => {
                    stack.push(target);
                    stack.extend(lo.iter().chain(hi.iter()).map(|b| &**b));
                }
                Self::AnyAll { expr, array, .. } => {
                    stack.push(expr);
                    stack.push(array);
                }
                Self::FunctionCall { args, .. } | Self::Array(args) => {
                    stack.extend(args.iter());
                }
                Self::AggregateOrdered {
                    call,
                    order_by,
                    filter,
                    ..
                } => {
                    stack.push(call);
                    stack.extend(order_by.iter().map(|o| &o.expr));
                    stack.extend(filter.iter().map(|b| &**b));
                }
                Self::WindowFunction {
                    args,
                    partition_by,
                    order_by,
                    filter,
                    ..
                } => {
                    stack.extend(args.iter().chain(partition_by.iter()));
                    stack.extend(order_by.iter().map(|(e, _, _)| e));
                    stack.extend(filter.iter().map(|b| &**b));
                }
                Self::InList { expr, list, .. } => {
                    stack.push(expr);
                    stack.extend(list.iter());
                }
                Self::Case {
                    operand,
                    branches,
                    else_branch,
                } => {
                    stack.extend(operand.iter().chain(else_branch.iter()).map(|b| &**b));
                    for (when, then) in branches {
                        stack.push(when);
                        stack.push(then);
                    }
                }
                Self::ScalarSubquery(s) | Self::Exists { subquery: s, .. } => f(s)?,
                Self::InSubquery { expr, subquery, .. } => {
                    stack.push(expr);
                    f(subquery)?;
                }
                Self::RowInSubquery { row, subquery, .. }
                | Self::RowCmpSubquery { row, subquery, .. } => {
                    stack.extend(row.iter());
                    f(subquery)?;
                }
            }
        }
        Ok(())
    }
}

/// v7.9.24 — LIMIT / OFFSET value. Integer literal at parse
/// time or a placeholder `$N` resolved during extended-query
/// Bind. mailrs migration follow-up H2.
///
/// v7.39 (round 305) — no longer `Copy`/`Eq`: the `Expr` variant boxes
/// an arbitrary row-count expression. Losing `Copy` is deliberate — it
/// made the compiler point at every site that used to duplicate a
/// row-count out of the AST, which is exactly the set that must not
/// bypass the resolution pre-pass.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitExpr {
    /// `LIMIT 10` — value known at parse time.
    Literal(u32),
    /// `LIMIT $N` — the 1-based parameter index, resolved against
    /// the bind values when the prepared statement executes.
    Placeholder(u16),
    /// v7.39 (round 305, V23) — `LIMIT (SELECT 4)` / `LIMIT
    /// greatest(2,3)`: a row-count expression that isn't constant, so
    /// it can't be folded at parse time. Evaluated once, before
    /// dispatch, by the engine's `resolve_limit_exprs` pre-pass, which
    /// rewrites it to `Literal` (or to `None` for a NULL result, PG's
    /// "no limit"). **No execution path may see this variant** —
    /// `as_literal` would report `None`, which every row-count reader
    /// takes to mean "unlimited", i.e. the whole table.
    Expr(alloc::boxed::Box<Expr>),
}

impl fmt::Display for LimitExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(n) => write!(f, "{n}"),
            Self::Placeholder(n) => write!(f, "${n}"),
            // Parenthesised so the round-trip text re-parses as one
            // row-count expression (`LIMIT (SELECT 4)`), which is also
            // the only spelling `FETCH FIRST` accepts.
            Self::Expr(e) => write!(f, "({e})"),
        }
    }
}

impl LimitExpr {
    /// Convenience for the simple-query path where no placeholders
    /// can possibly exist. Returns the literal value or `None` if
    /// this is a placeholder (caller must surface as Unsupported).
    ///
    /// v7.39 (round 305) — `None` is read by every row-count consumer as
    /// "no limit". An unresolved [`LimitExpr::Expr`] reaching here would
    /// therefore silently return the whole table, so the engine's
    /// `resolve_limit_exprs` pre-pass rewrites the variant away before
    /// dispatch. The assertion makes a missed nesting site fail loudly
    /// in every test build rather than quietly widening a result set.
    #[must_use]
    pub fn as_literal(&self) -> Option<u32> {
        match self {
            Self::Literal(n) => Some(*n),
            Self::Placeholder(_) => None,
            Self::Expr(_) => {
                debug_assert!(
                    false,
                    "LimitExpr::Expr reached execution — resolve_limit_exprs \
                     missed a nesting site; treating it as `no limit` would \
                     return every row"
                );
                None
            }
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
        self.limit.as_ref().and_then(LimitExpr::as_literal)
    }
    #[must_use]
    pub fn offset_literal(&self) -> Option<u32> {
        self.offset.as_ref().and_then(LimitExpr::as_literal)
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
    /// v7.38 (read01 U16) — `SEARCH { DEPTH | BREADTH } FIRST BY cols
    /// SET seqcol` on a recursive CTE. Desugared at parse time into an
    /// extra ordering column on the body (see `rewrite_search_and_cycle`).
    pub search: Option<SearchClause>,
    /// v7.38 (read01 U16) — `CYCLE cols SET markcol [TO v DEFAULT w]
    /// USING pathcol` cycle detection, desugared at parse time.
    pub cycle: Option<CycleClause>,
}

/// v7.38 (read01 U16) — parsed `SEARCH … FIRST BY … SET …` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchClause {
    /// `true` = DEPTH FIRST, `false` = BREADTH FIRST.
    pub depth_first: bool,
    /// The CTE output columns the search orders by.
    pub by_columns: Vec<String>,
    /// The new column holding the ordering key (a row-array for depth,
    /// a `(depth, keys…)` row for breadth).
    pub set_column: String,
}

/// v7.38 (read01 U16) — parsed `CYCLE … SET … [TO … DEFAULT …] USING …`.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleClause {
    /// Columns whose repetition along a path marks a cycle.
    pub columns: Vec<String>,
    /// The new boolean-ish column set to `mark_value` on a cycle.
    pub mark_column: String,
    /// Value written to `mark_column` when a cycle is detected (default
    /// `true`); `default_value` otherwise. PG allows any type; SPG carries
    /// them as literals.
    pub mark_value: Option<Literal>,
    pub default_value: Option<Literal>,
    /// The new column accumulating the visited-row path array.
    pub path_column: String,
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
    /// v7.39 (read01 round 149) — PG 17 allows MERGE as a
    /// data-modifying CTE body (`WITH m AS (MERGE … RETURNING …)`).
    Merge(Box<MergeStatement>),
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
    /// v7.39 (round 691) — an explicit `COLLATE` written on this key.
    /// It lives here rather than in the expression for the same reason
    /// `desc` does: at an ORDER BY key a collation is ordering
    /// information, and nothing downstream of the sort needs it. A new
    /// `Expr` variant would instead put a new arm on `eval_expr`, which
    /// this repo has measured to overflow the debug stack.
    ///
    /// `None` means none was written, and the key falls back to whatever
    /// its COLUMN declares — which is every key that existed before this.
    pub collation: Option<String>,
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
    /// v7.39 (read01 round 128) — qualified wildcard `qualifier.*`: every column
    /// of the table / alias `qualifier` (or, in a RETURNING list, the `OLD` /
    /// `NEW` pseudo-relation).
    QualifiedWildcard(String),
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    /// v7.39 (round 644) — `FROM ONLY t`: do not descend into `t`'s
    /// children.
    ///
    /// The keyword used to be absorbed at parse time, on the reasoning
    /// that SPG's inheritance children are separate relations a plain
    /// scan does not descend into — so ONLY already described what the
    /// scan did. That stopped being true when a partition parent
    /// started unioning its children: measured, `SELECT count(*) FROM
    /// ONLY <partitioned parent>` answered 2 where PG answers 0.
    pub only: bool,
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
    /// v7.39 (read01 partitionfuncs.c) — generic FROM-position table
    /// function channel: `(lowercase fn name, args)`. Carries
    /// `pg_partition_tree` / `pg_partition_ancestors`; the executor
    /// dispatches by name.
    pub table_fn_call: Option<Box<(String, Vec<Expr>)>>,
    /// v7.39 (read01 round 78) — this FROM item is a call to a function that
    /// returns a BASE type, so the item's row type IS that scalar: a whole-row
    /// reference to it yields the value, not a one-field composite
    /// (`SELECT j FROM jsonb_array_elements('[1]') AS j` → `1`, PG). The
    /// desugared shape is indistinguishable from a hand-written
    /// `FROM (SELECT unnest(…)) s`, which is a subquery and does NOT collapse —
    /// only the parser knows which one it built, so it says so here.
    pub scalar_fn_item: bool,
    /// v7.39 (read01 round 74) — `ROWS FROM (f(a), g(b))`: N table functions
    /// zipped in LOCKSTEP, the shorter padded with NULLs (the same rule the
    /// target-list SRFs follow — see round 67). The array-returning family keeps
    /// its own lowering; this channel carries the ones that have no array form
    /// (`generate_series`, a user `RETURNS SETOF` function).
    pub rows_from: Option<Vec<(String, Vec<Expr>)>>,
    /// v7.39 (round 205, JSON_TABLE epic) — a `JSON_TABLE(doc, '$path'
    /// COLUMNS (...))` FROM item. The doc expr may reference left-side
    /// tables (implicit LATERAL, like every SRF channel). Executed by
    /// walking the row path over the parsed doc, then each column's
    /// path per row-item; NESTED expands as a per-parent outer join.
    pub json_table: Option<Box<JsonTable>>,
}

/// What a FROM item IS.
///
/// v7.40.10 — a boolean cannot make a consumer handle a new kind; an
/// enum can. Every consumer that must know the difference writes a
/// `match` with no wildcard arm, so a variant added here is a compile
/// error at each of them rather than a defect at whichever one the new
/// shape reaches first.
///
/// The engine asked "is this item synthesised?" in fifty-six places and
/// exactly one of them listed every field. The rest were missing
/// between one and five, and the gaps were reachable:
/// `SELECT * FROM jsonb_each_text('{"a":1}'::jsonb)` answered
/// `relation "jsonb_each_text" does not exist` over the extended
/// protocol because one such list named four of seven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FromItemKind {
    /// A table, view or CTE named in the catalog.
    Relation,
    /// `unnest(...)` and the set-returning functions the parser lowers
    /// onto the same slot (`string_to_table`, `jsonb_object_keys`, …).
    Unnest,
    /// `generate_series(...)`.
    GenerateSeries,
    /// A derived table or LATERAL subquery.
    Subquery,
    /// `jsonb_each(...)` / `jsonb_each_text(...)`.
    JsonbEach,
    /// A table function call the parser kept as a name plus arguments.
    TableFn,
    /// `ROWS FROM (...)`.
    RowsFrom,
    /// `JSON_TABLE(...)`.
    JsonTable,
    /// A scalar function in FROM position, which yields one row.
    ScalarFn,
}

/// One walkable slot of a FROM item: an expression, or a nested SELECT.
///
/// v7.40.10 — see [`TableRef::try_for_each_slot_mut`].
#[derive(Debug)]
pub enum FromSlot<'a> {
    Expr(&'a mut Expr),
    Select(&'a mut SelectStatement),
}

/// The shared half of [`FromSlot`].
///
/// v7.40.11 — analysis reads a statement it must not modify, and Rust
/// has no way to write one walk that is generic over `&`/`&mut`. The two
/// bodies are the same destructure and must move together; the
/// destructures are total, so a new slot fails to compile in both.
#[derive(Debug)]
pub enum FromSlotRef<'a> {
    Expr(&'a Expr),
    Select(&'a SelectStatement),
}

impl TableRef {
    /// Whether this FROM item NAMES A RELATION — a table, view or CTE —
    /// rather than producing its own rows.
    ///
    /// **A total destructure, no `..`.** A field added to `TableRef` is a
    /// compile error here.
    ///
    /// v7.40.10, on evidence. This question is asked in 56 places across
    /// the engine and every one of them wrote its own list of fields.
    /// Exactly one was complete. The others were missing between one and
    /// five slots each, and each gap is a defect waiting for the shape
    /// that reaches it:
    ///
    /// ```text
    ///   try_stream_single_table's guard named four of seven, so
    ///   SELECT * FROM jsonb_each_text('{"a":1}'::jsonb)
    ///     ERROR:  relation "jsonb_each_text" does not exist
    ///   over the extended protocol, while count(*) over the same item
    ///   answered and the simple query protocol answered.
    /// ```
    ///
    /// `scalar_fn_item` counts as not-a-relation for the same reason the
    /// rest do: the row comes from the item, not from the catalog.
    #[must_use]
    pub fn names_a_relation(&self) -> bool {
        self.kind() == FromItemKind::Relation
    }

    /// What this FROM item is.
    ///
    /// **A total destructure, no `..`.** A field added to `TableRef` is
    /// a compile error here — which is the point, because every
    /// consumer matches exhaustively on the result.
    ///
    /// The slots are mutually exclusive by construction: the parser
    /// fills exactly one of them, or none for a plain relation.
    #[must_use]
    pub fn kind(&self) -> FromItemKind {
        let Self {
            name: _,
            alias: _,
            only: _,
            as_of_segment: _,
            unnest_column_aliases: _,
            with_ordinality: _,
            scalar_fn_item,
            unnest_expr,
            generate_series_args,
            lateral_subquery,
            jsonb_each_text_arg,
            table_fn_call,
            rows_from,
            json_table,
        } = self;
        if unnest_expr.is_some() {
            FromItemKind::Unnest
        } else if generate_series_args.is_some() {
            FromItemKind::GenerateSeries
        } else if lateral_subquery.is_some() {
            FromItemKind::Subquery
        } else if jsonb_each_text_arg.is_some() {
            FromItemKind::JsonbEach
        } else if table_fn_call.is_some() {
            FromItemKind::TableFn
        } else if rows_from.is_some() {
            FromItemKind::RowsFrom
        } else if json_table.is_some() {
            FromItemKind::JsonTable
        } else if *scalar_fn_item {
            FromItemKind::ScalarFn
        } else {
            FromItemKind::Relation
        }
    }

    /// Every expression this FROM item carries, and the SELECT nested in
    /// it — in one place, for every pass that needs them.
    ///
    /// **Written as a TOTAL destructure, with no `..`.** A field added
    /// to `TableRef` is a compile error here, rather than a defect in
    /// each pass that enumerated the slots for itself. That is the whole
    /// point of the function existing.
    ///
    /// v7.40.10, on evidence. `TableRef` carries seven expression slots
    /// and three separate passes each knew a different subset of them.
    /// In one day: the parameter-substitution walk knew only
    /// `lateral_subquery`, so `unnest($1)` reached execution still
    /// holding a placeholder (a customer's live 500); `describe` knew
    /// only `unnest_expr`, so `generate_series(…)` described no columns
    /// and a driver got a protocol error; and the LIMIT/OFFSET
    /// resolution knew CTEs and UNION peers but not a FROM subquery, so
    /// `LIMIT $n` inside a derived table returned every row.
    ///
    /// Fixing those three one at a time left four slots unvisited.
    /// Measured after the third fix shipped, all on the same message:
    ///
    /// ```text
    ///   jsonb_each_text($1)  parameter $1 referenced but only 0 bound
    ///   ROWS FROM (…$1…)     parameter $1 referenced but only 0 bound
    ///   json_table($1, …)    parameter $1 referenced but only 0 bound
    /// ```
    ///
    /// Those were the next three reports. This is what stops the fourth.
    ///
    /// # Errors
    /// Whatever the callbacks return; the walk stops at the first.
    pub fn try_for_each_slot_mut<E>(
        &mut self,
        visit: &mut dyn FnMut(FromSlot<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        // One callback rather than two, so a caller that needs the same
        // state for both — every caller so far — does not have to lend
        // it twice.
        let Self {
            // Not expressions — named so the destructure stays total.
            name: _,
            alias: _,
            only: _,
            as_of_segment: _,
            unnest_column_aliases: _,
            with_ordinality: _,
            scalar_fn_item: _,
            // The seven that carry something to walk.
            unnest_expr,
            generate_series_args,
            lateral_subquery,
            jsonb_each_text_arg,
            table_fn_call,
            rows_from,
            json_table,
        } = self;
        if let Some(e) = unnest_expr {
            visit(FromSlot::Expr(e))?;
        }
        if let Some(args) = generate_series_args {
            for a in args.iter_mut() {
                visit(FromSlot::Expr(a))?;
            }
        }
        if let Some(sub) = lateral_subquery {
            visit(FromSlot::Select(sub))?;
        }
        if let Some((_, e)) = jsonb_each_text_arg {
            visit(FromSlot::Expr(e))?;
        }
        if let Some(call) = table_fn_call {
            for a in call.1.iter_mut() {
                visit(FromSlot::Expr(a))?;
            }
        }
        if let Some(items) = rows_from {
            for (_, args) in items.iter_mut() {
                for a in args.iter_mut() {
                    visit(FromSlot::Expr(a))?;
                }
            }
        }
        if let Some(jt) = json_table {
            let JsonTable {
                doc,
                row_path: _,
                columns: _,
                passing,
            } = jt.as_mut();
            visit(FromSlot::Expr(doc))?;
            for (_, e) in passing.iter_mut() {
                visit(FromSlot::Expr(e))?;
            }
        }
        Ok(())
    }

    /// The shared twin of [`Self::try_for_each_slot_mut`], for analysis
    /// that reads a statement rather than rewriting it. Same total
    /// destructure — a new slot is a compile error in both.
    ///
    /// # Errors
    /// Whatever `visit` returns.
    pub fn try_for_each_slot<'a, E>(
        &'a self,
        visit: &mut dyn FnMut(FromSlotRef<'a>) -> Result<(), E>,
    ) -> Result<(), E> {
        let Self {
            name: _,
            alias: _,
            only: _,
            as_of_segment: _,
            unnest_column_aliases: _,
            with_ordinality: _,
            scalar_fn_item: _,
            unnest_expr,
            generate_series_args,
            lateral_subquery,
            jsonb_each_text_arg,
            table_fn_call,
            rows_from,
            json_table,
        } = self;
        if let Some(e) = unnest_expr {
            visit(FromSlotRef::Expr(e))?;
        }
        if let Some(args) = generate_series_args {
            for a in args {
                visit(FromSlotRef::Expr(a))?;
            }
        }
        if let Some(sub) = lateral_subquery {
            visit(FromSlotRef::Select(sub))?;
        }
        if let Some((_, e)) = jsonb_each_text_arg {
            visit(FromSlotRef::Expr(e))?;
        }
        if let Some(call) = table_fn_call {
            for a in &call.1 {
                visit(FromSlotRef::Expr(a))?;
            }
        }
        if let Some(items) = rows_from {
            for (_, args) in items {
                for a in args {
                    visit(FromSlotRef::Expr(a))?;
                }
            }
        }
        if let Some(jt) = json_table {
            let JsonTable {
                doc,
                row_path: _,
                columns: _,
                passing,
            } = jt.as_ref();
            visit(FromSlotRef::Expr(doc))?;
            for (_, e) in passing {
                visit(FromSlotRef::Expr(e))?;
            }
        }
        Ok(())
    }
}

/// v7.39 (round 205) — a `JSON_TABLE` FROM item.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonTable {
    /// The document expression (jsonb/json/text). May reference outer
    /// columns → implicit LATERAL.
    pub doc: Box<Expr>,
    /// The row-pattern jsonpath (the 2nd JSON_TABLE argument); each
    /// match is one row's context item.
    pub row_path: String,
    /// The COLUMNS list (regular columns, FOR ORDINALITY, NESTED).
    pub columns: Vec<JsonTableColumn>,
    /// `PASSING <expr> AS <name>` variables, folded into jsonpath `$name`.
    pub passing: Vec<(String, Expr)>,
}

/// v7.39 (round 205) — one entry in a JSON_TABLE (or NESTED) COLUMNS list.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonTableColumn {
    /// `<name> FOR ORDINALITY` — 1-based counter within this level.
    Ordinality { name: String },
    /// `<name> <type> [FORMAT JSON] PATH '<p>' [WITH WRAPPER]
    /// [{DEFAULT <e>|ERROR|NULL} ON EMPTY] [... ON ERROR]`, or
    /// `<name> <type> EXISTS [PATH '<p>']`.
    Regular {
        name: String,
        ty: ColumnTypeName,
        /// The column jsonpath; defaults to `$.<name>` when `PATH` omitted.
        path: String,
        /// `EXISTS [PATH …]` — the column is a boolean "did the path match".
        exists: bool,
        /// `FORMAT JSON` — return the raw jsonb value (not a coerced scalar).
        format_json: bool,
        /// `WITH [UNCONDITIONAL] WRAPPER` — wrap the result in a json array.
        wrapper: bool,
        /// Behaviour when the path matches nothing (default NULL).
        on_empty: JsonTableOnBehavior,
        /// Behaviour when coercion fails (default NULL).
        on_error: JsonTableOnBehavior,
    },
    /// `NESTED PATH '<p>' COLUMNS (...)` — a child level joined per parent
    /// row like a LEFT JOIN (a parent with no nested match still emits one
    /// row, nested cols NULL).
    Nested {
        path: String,
        columns: Vec<JsonTableColumn>,
    },
}

/// v7.39 (round 205) — a JSON_TABLE column's ON EMPTY / ON ERROR clause.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonTableOnBehavior {
    /// Default: the column value is NULL.
    Null,
    /// `ERROR ON {EMPTY|ERROR}` — raise PG's error.
    Error,
    /// `DEFAULT <expr> ON {EMPTY|ERROR}` — the given value.
    Default(Box<Expr>),
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
    /// v7.39 (round 725) — SEMI join: each drive row is kept AT MOST
    /// once, paired with the first peer row that satisfies the ON. Not
    /// reachable from SQL — the EXISTS pull-up emits it, which is what
    /// frees positive EXISTS from the round-721 uniqueness gate (an
    /// INNER join would multiply the outer rows; a semi join cannot).
    Semi,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    /// v7.39.2 — `<expr> COLLATE <name>`: the collation this expression
    /// compares under, whatever the column or the database says.
    ///
    /// The parser used to refuse the locale names in this position and
    /// SILENTLY ABSORB the byte-order ones, so `'a' COLLATE "C" < 'B'`
    /// answered `t` where PostgreSQL 18.6 answers `f` — the one family
    /// it let through is the one where dropping it changes the answer.
    ///
    /// Whether dropping is safe depends on the DATABASE's own collation,
    /// which the parser cannot see: under `SPG_LC_COLLATE=C` absorbing
    /// `COLLATE "C"` is exactly right. So the name rides along and the
    /// engine, which knows, decides.
    Collate {
        expr: Box<Expr>,
        collation: String,
    },
    Column(ColumnName),
    /// v7.39 (read01 round 77) — a NAMED call argument (`f(x := 1)`, or the
    /// older `f(x => 1)` spelling). Which slot the name fills depends on the
    /// callee's declared parameter names, and a user function's live in the
    /// catalog — which the parser cannot see. So the name rides along in the
    /// tree and the evaluator, which has the catalog, does the reordering.
    /// Appears only inside a `FunctionCall`'s argument list.
    NamedArg {
        name: String,
        expr: Box<Expr>,
    },
    /// v7.39 (read01 round 100) — `VARIADIC <array>` as the last argument of a
    /// variadic function call (`concat_ws(',', VARIADIC ARRAY[…])`). The inner
    /// expression evaluates to an array whose elements the evaluator splices
    /// into the call as individual trailing arguments. Appears only inside a
    /// `FunctionCall`'s argument list.
    Variadic(Box<Expr>),
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
    /// v7.38 (read01, T9) — composite field access `(expr).field`. `base`
    /// evaluates to a composite/record value (an explicit `ROW(...)`, a
    /// whole-row reference, or a composite-returning function); `field` names
    /// the member (`f1`..`fN` positional for an anonymous ROW, or the base
    /// column names for a whole-row). Only the parenthesised form reaches
    /// here — a bare `a.b` is parsed as a qualified column reference.
    FieldAccess {
        base: Box<Expr>,
        field: String,
    },
    /// Postfix `IS NULL` / `IS NOT NULL`. Returns BOOL.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// v7.39 (round 328, V45) — `x IS [NOT] TRUE | FALSE | UNKNOWN`, the
    /// three-valued boolean tests. `value` is `Some(true)` for TRUE,
    /// `Some(false)` for FALSE and `None` for UNKNOWN.
    ///
    /// These used to be lowered to `CASE` / `IS NULL` right in the parser.
    /// The semantics were right, but the AST then had no way to say what
    /// the user wrote, so every renderer printed the lowering:
    /// `CHECK ((a > 1) IS TRUE)` came back as
    /// `CHECK ((CASE WHEN (a > 1) THEN TRUE ELSE FALSE END))`, and a
    /// dumped view lost the form too.
    BoolTest {
        expr: Box<Expr>,
        value: Option<bool>,
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
    /// `(a, b, …) [NOT] IN (SELECT x, y, …)` — a row constructor tested
    /// against a multi-column subquery. Row comparisons against a *list*
    /// decompose to OR-of-AND at parse time, but the subquery form can't
    /// (its rows are only known at runtime), so this survives as its own
    /// node evaluated with PG's row-comparison three-valued logic.
    RowInSubquery {
        row: Vec<Expr>,
        subquery: Box<SelectStatement>,
        negated: bool,
    },
    /// `(a, b, …) <op> (SELECT x, y, …)` — a row constructor compared to a
    /// single-row subquery (`=`, `<>`, `<`, `<=`, `>`, `>=`). Like
    /// RowInSubquery, the literal-RHS form decomposes at parse time but the
    /// subquery form can't, so it survives as its own node. The subquery
    /// must yield at most one row (zero → NULL, PG scalar-subquery rule).
    RowCmpSubquery {
        row: Vec<Expr>,
        op: BinOp,
        subquery: Box<SelectStatement>,
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
    /// `RANGE BETWEEN <interval> PRECEDING …` — value-based offset over a
    /// DATE / TIMESTAMP ORDER BY column (PG time-series windows). The
    /// interval is folded to its (months, days, micros) components at
    /// parse time.
    IntervalPreceding {
        months: i32,
        days: i32,
        micros: i64,
    },
    IntervalFollowing {
        months: i32,
        days: i32,
        micros: i64,
    },
}

impl fmt::Display for FrameBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnboundedPreceding => f.write_str("UNBOUNDED PRECEDING"),
            Self::OffsetPreceding(n) => write!(f, "{n} PRECEDING"),
            Self::CurrentRow => f.write_str("CURRENT ROW"),
            Self::OffsetFollowing(n) => write!(f, "{n} FOLLOWING"),
            Self::UnboundedFollowing => f.write_str("UNBOUNDED FOLLOWING"),
            Self::IntervalPreceding { .. } => f.write_str("INTERVAL PRECEDING"),
            Self::IntervalFollowing { .. } => f.write_str("INTERVAL FOLLOWING"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// v7.39 (round 253) — a field name the parser does not know. PG
    /// resolves EXTRACT fields at RUNTIME and reports them with the
    /// source type (`unit "nosuch" not recognized for type timestamp
    /// without time zone`, 22023), so the parser carries the raw name
    /// instead of rejecting.
    Other(String),
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
            Self::Other(name) => return f.write_str(name),
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
    /// Exact decimal literal — a bare `12.34`-style token, kept as
    /// `unscaled / 10^scale` so no precision or trailing-zero scale is lost
    /// before it becomes a `Value::Numeric`. PG parses such literals as
    /// `numeric`, not `double precision`. (Scientific/huge literals stay
    /// `Float`.)
    Numeric {
        unscaled: i128,
        /// v7.39 (round 271) — widened to u16. At u8 a literal with more
        /// than 255 decimal places could not be represented, and the
        /// conversion's `.expect("lexer-validated decimal")` aborted the
        /// query with an internal error on SQL PG accepts.
        scale: u16,
    },
    /// v7.38 (read01, T3.C3) — an exact decimal literal whose mantissa overflows
    /// i128 (kept as its source digit string, `[-]digits[.digits]`). Becomes a
    /// `Value::NumericBig` at eval; previously such literals fell back to double.
    NumericBig(String),
    String(String),
    /// v7.38.8 — a temporal constant that has already been decoded.
    ///
    /// Without these the only way to carry one through the AST was as
    /// text, and a predicate comparing a `timestamp` column against a
    /// literal then coerced that text back into a timestamp ONCE PER
    /// ROW — 32 ns of the 52 a comparison cost, measured on a customer
    /// profile. `constfold` produced text for the same reason: its exit
    /// had nothing else to hand back.
    ///
    /// `text` keeps the spelling so `Display` round-trips byte for byte,
    /// the way `Interval` already does and for the same reason: this
    /// node is printed in EXPLAIN, in dumps and in error messages, and
    /// none of those should change because the value stopped being
    /// carried as a string. The enum already holds a `String` and an
    /// `i128`, so neither variant widens it.
    Timestamp {
        micros: i64,
        text: String,
    },
    /// Days since the epoch `Value::Date` counts from. See
    /// [`Literal::Timestamp`].
    Date {
        days: i32,
        text: String,
    },
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
    /// v7.39 (round 353, M9) — MySQL's `DIV`: integer division that
    /// truncates TOWARD ZERO (`-7 DIV 2` is -3, measured on MariaDB 11)
    /// and answers NULL on a zero divisor. MySQL-dialect only; `/` there
    /// is a real division (round 351).
    IntDiv,
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
    /// v7.39 (read01 geo_ops.c) — `?||` geometric "is parallel".
    GeomParallel,
    /// v7.39 (read01 rangetypes.c) — range `&<` / `&>`.
    OverLeft,
    OverRight,
    /// v7.39 (read01 geo_ops.c) — `?-|` geometric "is perpendicular".
    GeomPerp,
    /// v7.39 (read01 geo_ops.c) — `~=` geometric "same as".
    GeomSameAs,
    /// v7.39 (read01 geo_ops.c) — `##` closest point on the right-hand
    /// object to the left-hand one.
    ClosestPoint,
    /// v7.39 (read01 geo_ops.c) — `?-` points horizontally aligned.
    GeomHoriz,
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
    /// v7.39 (round 407) — MySQL's logical `XOR` operator. Reads both
    /// sides as truth values and returns their exclusive-or (`1 XOR 0`
    /// is 1, `1 XOR 1` is 0); NULL on either side yields NULL. Only the
    /// MySQL dialect produces it; PG has no logical XOR. Its precedence
    /// sits between OR (loosest) and AND.
    LogicalXor,
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
    /// `@?` — jsonb path existence (jsonb_path_exists).
    JsonPathExists,
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
    /// v7.39 (round 508) — `?#`, "do these intersect": box/box, line/box,
    /// line/line, lseg/box, lseg/line, lseg/lseg, path/path.
    Intersects,
    /// v7.39 (round 508) — `<^` / `>^`, strictly below / strictly above
    /// (point, box).
    IsBelow,
    IsAbove,
    /// v7.39 (round 508) — the `text_pattern_ops` comparisons `~<~`, `~<=~`,
    /// `~>~`, `~>=~`: BYTE order, ignoring collation. `'A' ~<~ 'a'` is true
    /// where `'A' < 'a'` is false under a non-C collation, which is the
    /// whole reason the operator family exists — it is what makes a LIKE
    /// prefix index-usable. pg_dump writes these into index definitions.
    PatternLt,
    PatternLtEq,
    PatternGt,
    PatternGtEq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
    /// Bitwise NOT `~` on integers.
    BitNot,
    /// v7.39 (round 507) — unary `+`. SPG had no such operator at all:
    /// `SELECT +1` parsed only because the lexer reads `+1` as one signed
    /// literal, so `+ 1`, `+a`, `+(1)` and `1 + +1` were all syntax errors
    /// while PG18 and MariaDB accept every one of them.
    ///
    /// It is not a no-op to drop at parse time — PG refuses it on
    /// non-numeric operands ("operator does not exist: + boolean"), so the
    /// operand's type has to be seen at eval.
    Plus,
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
    /// v7.39 (round 435) — does this statement implicitly COMMIT an open
    /// transaction under MySQL?
    ///
    /// PG runs DDL inside the transaction; MySQL commits before (and after)
    /// it, so `START TRANSACTION; INSERT …; CREATE TABLE …; ROLLBACK` keeps
    /// the INSERT on MySQL and loses it on PG. Measured on MariaDB 11 for
    /// CREATE TABLE, ALTER TABLE, DROP TABLE, TRUNCATE, CREATE INDEX and a
    /// nested START TRANSACTION; and measured NOT to fire for `CREATE
    /// TEMPORARY TABLE`, `SET`, or a SELECT.
    ///
    /// A positive list, not "everything that is not DML": a statement
    /// wrongly listed here commits a client's data early, which is as bad as
    /// the divergence it fixes. SPG-only maintenance verbs (VACUUM, DISCARD,
    /// COMPACT) are left out — a MySQL session never sends them.
    #[must_use]
    pub fn mysql_implicit_commit(&self) -> bool {
        match self {
            // MySQL's documented exception, measured on MariaDB 11: a
            // TEMPORARY table is not DDL for this purpose and does not
            // commit. (Round 435 got this for free because the parser then
            // lowered that spelling to `Statement::Empty`; round 436 made it
            // a real CREATE TABLE, and the round-435 pin caught it.)
            Self::CreateTable(c) => !c.temporary,
            // MySQL commits the open transaction and opens a fresh one.
            Self::Begin { .. }
            | Self::DropTable { .. }
            | Self::DropIndex { .. }
            | Self::CreateIndex(_)
            | Self::AlterIndex { .. }
            | Self::AlterTable(_)
            | Self::Truncate { .. }
            | Self::Analyze { .. }
            | Self::CreateStatistics { .. }
            | Self::DropStatistics { .. }
            | Self::CreateView { .. }
            | Self::DropView { .. }
            | Self::CreateMaterializedView { .. }
            | Self::RefreshMaterializedView { .. }
            | Self::DropMaterializedView { .. }
            | Self::CreateSequence(_)
            | Self::AlterSequence { .. }
            | Self::DropSequence { .. }
            | Self::CreateFunction(_)
            | Self::DropFunction { .. }
            | Self::CreateTrigger(_)
            | Self::DropTrigger { .. }
            | Self::CreateRule(_)
            | Self::DropRule { .. }
            | Self::CreateType(_)
            | Self::DropType { .. }
            | Self::AlterTypeAddValue { .. }
            | Self::AlterTypeRenameValue { .. }
            | Self::CreateDomain(_)
            | Self::AlterDomain { .. }
            | Self::DropDomain { .. }
            | Self::CreateSchema { .. }
            | Self::DropSchema { .. }
            | Self::CreateUser { .. }
            | Self::DropUser { .. }
            | Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::CreatePolicy(_)
            | Self::AlterPolicy(_)
            | Self::DropPolicy { .. }
            | Self::CommentOn { .. }
            | Self::CreateExtension { .. } => true,
            _ => false,
        }
    }

    #[must_use]
    pub fn is_readonly(&self) -> bool {
        match self {
            Statement::RenameTables(_) => false,
            // v7.39 (round 288) — SET CONSTRAINTS changes transaction
            // state, and IMMEDIATE can run the deferred checks there and
            // then; writer-path.
            Statement::SetConstraints { .. } => false,
            // v7.39 (round 695) — it writes nothing (SPG has no
            // postgresql.auto.conf), but PG classes ALTER SYSTEM as a
            // writer and a read-only session refuses it there too.
            Statement::AlterSystem { .. } => false,
            // Same shape: a no-op here, a writer to PG, so a read-only
            // session refuses it as PG's would.
            Statement::NoOpPreventedInTransaction { .. } => false,
            Statement::DropDatabase { .. } => false,
            // v7.39 (round 696) — they perform nothing, so nothing is
            // written; PG classes LOCK and the OWNED BY pair as writers and
            // a read-only session refuses them there.
            Statement::ValidateOnly { .. } => false,
            // v7.39 (round 750) — a credential rotation persists.
            Statement::AlterRolePassword { .. } => true,
            Statement::DropAggregate { .. } => false,
            // v7.39 (round 547) — records a GUC default in the catalog.
            Statement::SetDbRoleSetting(_) => false,
            // v7.39 (round 535) — REINDEX / CLUSTER rebuild nothing here,
            // but they name a relation and PG refuses one that is not
            // there, so they are not read-only in the sense this asks.
            Statement::Maintain { .. } => false,
            // v7.39 (round 277) — the prepared-statement surface is
            // session state, like SET; writer-path so it lands on the
            // engine that owns the session. EXECUTE may also run a
            // write, and its body is only known at execution time.
            Statement::Prepare { .. }
            | Statement::Execute { .. }
            | Statement::Deallocate(_)
            | Statement::Call(_)
            | Statement::PrepareTransaction(_)
            | Statement::CreateStatistics { .. }
            | Statement::DropStatistics { .. }
            // v7.39 (round 318, V51) — KILL signals another connection;
            // it must run on the writer path that owns the registry hook.
            | Statement::Kill { .. }
            // v7.39 (round 320, V53) — DISCARD throws session state away;
            // writer path, like SET / RESET.
            | Statement::Discard(_)
            // v7.39.2 — `USE <db>` writes session state, the same way
            // SET does, and takes the same path.
            | Statement::UseDatabase(_) => false,
            // v7.39 (round 295, E3 Phase 1b) — a SELECT that asks for row
            // locks MUTATES the lock table, so it is not a read. Left as
            // a read it went to the read-only executor and the locking
            // pre-pass never ran at all — the clause was honoured only
            // inside an explicit transaction, and silently ignored in
            // autocommit, which is where a queue worker runs it.
            Statement::Select(s) if s.locking.is_some() => false,
            Statement::Select(_)
            | Statement::CopyTo { .. }
            | Statement::CopyToFile { .. }
            | Statement::Explain(_)
            | Statement::ShowTables
            | Statement::ShowDatabases
            | Statement::ShowCreateTable(_)
            | Statement::ShowIndexes(_)
            | Statement::ShowStatus
            | Statement::ShowVariables
            | Statement::ShowVariablesLike(_)
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
            // v7.39 (round 169) — VACUUM mutates storage (reclaims
            // tombstoned versions): writer path.
            | Statement::Vacuum { .. }
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
            | Statement::Begin(_)
            | Statement::Commit
            | Statement::Rollback
            | Statement::Savepoint(_)
            | Statement::RollbackToSavepoint(_)
            | Statement::ReleaseSavepoint(_)
            | Statement::CreateUser(_)
            | Statement::DropUser { .. }
            | Statement::SetRole(_)
            | Statement::Grant(_)
            | Statement::Revoke(_)
            | Statement::CreatePolicy(_)
            | Statement::AlterPolicy(_)
            | Statement::DropPolicy(_)
            | Statement::AlterIndex(_)
            | Statement::AlterTable(_)
            | Statement::CreatePublication(_)
            | Statement::DropPublication { .. }
            | Statement::CreateSubscription(_)
            | Statement::DropSubscription { .. }
            | Statement::Analyze(_)
            | Statement::Truncate { .. }
            | Statement::CompactColdSegments
            | Statement::SetParameter { .. }
            | Statement::SetParameterList(_)
            | Statement::SetUserVars(..)
            | Statement::SetTransaction { .. }
            | Statement::ShowParameter(_)
            | Statement::ResetParameter(_)
            | Statement::CreateFunction(_)
            | Statement::CreateTrigger(_)
            | Statement::DropTrigger { .. }
            | Statement::CreateRule(_)
            | Statement::DropRule { .. }
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
            | Statement::AlterTypeRenameValue { .. }
            | Statement::CommentOn { .. }
            | Statement::DropType { .. }
            | Statement::CreateDomain(_)
            | Statement::DropDomain { .. }
            | Statement::CreateSchema { .. }
            | Statement::DropSchema { .. }
            // v7.39 (round 218) — cursors mutate per-session cursor state
            // (open/position/close) on the writer engine: writer path.
            | Statement::DeclareCursor { .. }
            | Statement::FetchCursor { .. }
            | Statement::MoveCursor { .. }
            | Statement::CloseCursor { .. }
            // v7.39 (round 222) — LISTEN/NOTIFY mutate session channel
            // state / the notification queue: writer path.
            | Statement::Listen(_)
            | Statement::Notify { .. }
            | Statement::Unlisten(_)
            | Statement::CopyFromFile { .. }
            | Statement::AlterDomain { .. } => false,
        }
    }
}

/// v7.39 (read01 round 57) — a parsed GRANT / REVOKE. The same shape serves
/// both; `Statement::Grant` vs `Statement::Revoke` says which way it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantStatement {
    /// The privileges. EMPTY = `ALL [PRIVILEGES]`. In the role-membership shape
    /// (`GRANT devs TO alice`, no ON clause) these words are ROLE NAMES, which
    /// is why they keep the case the user typed.
    pub privileges: Vec<GrantPriv>,
    /// What the privileges are on.
    pub object: GrantObject,
    /// The roles granted to / revoked from. An empty string entry = PUBLIC.
    pub grantees: Vec<String>,
    /// GRANT: a trailing `WITH GRANT OPTION`. REVOKE: a leading
    /// `GRANT OPTION FOR` (revoke only the right to re-grant, keep the
    /// privilege itself).
    pub grant_option: bool,
}

/// v7.39 (read01 round 59) — one privilege in a GRANT, with the optional COLUMN
/// list PG allows per privilege: `GRANT SELECT (a, b), INSERT (c) ON t TO dan`.
/// An empty column list means the privilege is table-wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPriv {
    pub word: String,
    pub columns: Vec<String>,
}

/// v7.39 (read01 round 57) — the object a GRANT names. SPG enforces TABLE
/// privileges; every other object class parses and is accepted as a no-op, so
/// a pg_dump that grants on schemas / sequences / functions still restores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantObject {
    /// `ON [TABLE] a, b` — the enforced case.
    Tables(Vec<String>),
    /// v7.39 (read01 round 58) — `GRANT devs TO alice` / `REVOKE devs FROM
    /// alice`: role MEMBERSHIP, which has no ON clause at all. Carries the
    /// granted roles; the grantees are the members.
    Roles(Vec<String>),
    /// v7.39 (read01 round 60) — `ON SEQUENCE a, b`. A sequence's meaningful
    /// privileges are SELECT (currval), UPDATE (setval) and USAGE (nextval).
    Sequences(Vec<String>),
    /// v7.39 (read01 round 60) — `ON SCHEMA public`. USAGE / CREATE.
    Schemas(Vec<String>),
    /// v7.39 (read01 round 60) — `ON DATABASE app`. CREATE / CONNECT / TEMP.
    Databases(Vec<String>),
    /// v7.39 (read01 round 61) — `ON FUNCTION f(int)`. The names are bare
    /// (SPG keys functions by name); the argument list parses and is dropped.
    Functions(Vec<(String, Option<Vec<String>>)>),
    /// v7.39 (read01 round 61) — `ON ALL TABLES IN SCHEMA public`: expands to
    /// every table at GRANT time, exactly like PG.
    AllTablesInSchema,
    /// `ON TYPE / LANGUAGE / …`. Carries the object-class word for the no-op
    /// message.
    Other(String),
}

impl GrantStatement {
    /// Round-trip text. `grant = false` renders the REVOKE form.
    fn render(&self, grant: bool) -> alloc::string::String {
        use core::fmt::Write as _;
        let mut s = alloc::string::String::new();
        let privs = if self.privileges.is_empty() {
            alloc::string::String::from("ALL")
        } else {
            let parts: Vec<_> = self
                .privileges
                .iter()
                .map(|p| {
                    if p.columns.is_empty() {
                        p.word.clone()
                    } else {
                        let cols: Vec<_> = p.columns.iter().map(|c| quote_ident(c)).collect();
                        alloc::format!("{} ({})", p.word, cols.join(", "))
                    }
                })
                .collect();
            parts.join(", ")
        };
        let obj = match &self.object {
            GrantObject::Tables(t) => {
                let names: Vec<_> = t.iter().map(|n| quote_ident(n)).collect();
                alloc::format!("TABLE {}", names.join(", "))
            }
            GrantObject::Roles(r) => {
                let names: Vec<_> = r.iter().map(|n| quote_ident(n)).collect();
                names.join(", ")
            }
            GrantObject::Sequences(n) => {
                let names: Vec<_> = n.iter().map(|x| quote_ident(x)).collect();
                alloc::format!("SEQUENCE {}", names.join(", "))
            }
            GrantObject::Schemas(n) => {
                let names: Vec<_> = n.iter().map(|x| quote_ident(x)).collect();
                alloc::format!("SCHEMA {}", names.join(", "))
            }
            GrantObject::Databases(n) => {
                let names: Vec<_> = n.iter().map(|x| quote_ident(x)).collect();
                alloc::format!("DATABASE {}", names.join(", "))
            }
            GrantObject::Functions(n) => {
                let names: Vec<_> = n
                    .iter()
                    .map(|(name, args)| match args {
                        Some(a) => alloc::format!("{}({})", quote_ident(name), a.join(", ")),
                        None => quote_ident(name),
                    })
                    .collect();
                alloc::format!("FUNCTION {}", names.join(", "))
            }
            GrantObject::AllTablesInSchema => "ALL TABLES IN SCHEMA public".into(),
            GrantObject::Other(k) => k.clone(),
        };
        let who: Vec<_> = self
            .grantees
            .iter()
            .map(|g| {
                if g.is_empty() {
                    "PUBLIC".into()
                } else {
                    quote_ident(g)
                }
            })
            .collect();
        if let GrantObject::Roles(_) = &self.object {
            let _ = if grant {
                write!(s, "GRANT {obj} TO {}", who.join(", "))
            } else {
                write!(s, "REVOKE {obj} FROM {}", who.join(", "))
            };
            return s;
        }
        if grant {
            let _ = write!(s, "GRANT {privs} ON {obj} TO {}", who.join(", "));
            if self.grant_option {
                s.push_str(" WITH GRANT OPTION");
            }
        } else {
            s.push_str("REVOKE ");
            if self.grant_option {
                s.push_str("GRANT OPTION FOR ");
            }
            let _ = write!(s, "{privs} ON {obj} FROM {}", who.join(", "));
        }
        s
    }
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => Ok(()),
            // v7.39 (round 695) — deparsed the way PG writes it.
            // v7.39 (round 696) — never deparsed into a dump (nothing is
            // stored), so the shortest faithful spelling of what it was.
            Self::DropAggregate { if_exists, items } => {
                f.write_str("DROP AGGREGATE ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, (name, args)) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    match args {
                        Some(a) => write!(f, "{name}({})", a.join(", "))?,
                        None => write!(f, "{name}(*)")?,
                    }
                }
                Ok(())
            }
            Self::AlterRolePassword { name, password } => {
                write!(f, "ALTER ROLE {}", quote_ident(name))?;
                match password {
                    Some(_) => f.write_str(" PASSWORD '<redacted>'"),
                    None => f.write_str(" PASSWORD NULL"),
                }
            }
            Self::ValidateOnly { kind, names } => match kind {
                ValidateOnlyKind::LockTable => write!(f, "LOCK TABLE {}", names.join(", ")),
                ValidateOnlyKind::RoleName => {
                    write!(f, "DROP OWNED BY {}", names.join(", "))
                }
                ValidateOnlyKind::SecurityLabel => f.write_str("SECURITY LABEL"),
                ValidateOnlyKind::ExtensionAvailable => {
                    write!(f, "CREATE EXTENSION {}", names.join(", "))
                }
                ValidateOnlyKind::ForeignInfra => f.write_str("CREATE SERVER"),
                ValidateOnlyKind::CollationName => {
                    write!(f, "DROP COLLATION {}", names.join(", "))
                }
                ValidateOnlyKind::TsConfigName => {
                    write!(f, "DROP TEXT SEARCH CONFIGURATION {}", names.join(", "))
                }
                ValidateOnlyKind::EventTriggerName => {
                    write!(f, "DROP EVENT TRIGGER {}", names.join(", "))
                }
                ValidateOnlyKind::TablespaceName => {
                    write!(f, "DROP TABLESPACE {}", names.join(", "))
                }
                ValidateOnlyKind::LargeObjectOid => {
                    write!(f, "ALTER LARGE OBJECT {}", names.join(", "))
                }
                ValidateOnlyKind::TypeName => write!(f, "ALTER TYPE {}", names.join(", ")),
                ValidateOnlyKind::AggregateName => {
                    write!(f, "ALTER AGGREGATE {}", names.join(", "))
                }
                ValidateOnlyKind::ConversionName => {
                    write!(f, "DROP CONVERSION {}", names.join(", "))
                }
                ValidateOnlyKind::LanguageName => {
                    write!(f, "DROP LANGUAGE {}", names.join(", "))
                }
                ValidateOnlyKind::ExtensionInstalled => {
                    write!(f, "DROP EXTENSION {}", names.join(", "))
                }
            },
            Self::AlterSystem { parameter } => match parameter {
                Some(p) => write!(f, "ALTER SYSTEM RESET {p}"),
                None => f.write_str("ALTER SYSTEM RESET ALL"),
            },
            // v7.39 (round 547) — round-trips as PG writes it.
            Self::SetDbRoleSetting(st) => {
                match (&st.database, &st.role) {
                    (Some(d), None) => write!(f, "ALTER DATABASE {d}")?,
                    (_, Some(r)) => write!(f, "ALTER ROLE {r}")?,
                    (None, None) => f.write_str("ALTER ROLE ALL")?,
                }
                if let (Some(d), Some(_)) = (&st.database, &st.role) {
                    write!(f, " IN DATABASE {d}")?;
                }
                match (&st.param, &st.value) {
                    (None, _) => f.write_str(" RESET ALL"),
                    (Some(p), None) => write!(f, " RESET {p}"),
                    (Some(p), Some(v)) => write!(f, " SET {p} = '{v}'"),
                }
            }
            Self::Maintain {
                kind,
                concurrently,
                target,
            } => {
                f.write_str(match kind {
                    crate::ast::MaintainKind::ClusterRelation => "CLUSTER ",
                    _ => "REINDEX ",
                })?;
                if *concurrently {
                    f.write_str("CONCURRENTLY ")?;
                }
                if let Some(t) = target {
                    f.write_str(t)?;
                }
                Ok(())
            }
            Self::DropDatabase { name, if_exists } => {
                f.write_str("DROP DATABASE ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                f.write_str(name)
            }
            Self::NoOpPreventedInTransaction { what, .. } => f.write_str(what),
            Self::SetConstraints { names, deferred } => {
                f.write_str("SET CONSTRAINTS ")?;
                if names.is_empty() {
                    f.write_str("ALL")?;
                } else {
                    for (i, n) in names.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        f.write_str(n)?;
                    }
                }
                f.write_str(if *deferred { " DEFERRED" } else { " IMMEDIATE" })
            }
            // v7.39 (round 277) — the source text is kept verbatim so
            // `pg_prepared_statements.statement` can report it the way
            // PG does (the whole PREPARE statement, not just the body).
            Self::Prepare { source, .. } => f.write_str(source),
            Self::Execute { name, args } => {
                write!(f, "EXECUTE {}", quote_ident(name))?;
                if !args.is_empty() {
                    f.write_str("(")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{a}")?;
                    }
                    f.write_str(")")?;
                }
                Ok(())
            }
            Self::CreateStatistics {
                name,
                if_not_exists,
                kinds,
                columns,
                table,
            } => {
                f.write_str("CREATE STATISTICS ")?;
                if *if_not_exists {
                    f.write_str("IF NOT EXISTS ")?;
                }
                write!(f, "{}", quote_ident(name))?;
                if !kinds.is_empty() {
                    write!(f, " ({})", kinds.join(", "))?;
                }
                write!(f, " ON {} FROM {}", columns.join(", "), quote_ident(table))
            }
            Self::DropStatistics { name, if_exists } => {
                f.write_str("DROP STATISTICS ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                write!(f, "{}", quote_ident(name))
            }
            Self::Call(n) => write!(f, "CALL {}()", quote_ident(n)),
            Self::PrepareTransaction(gid) => write!(f, "PREPARE TRANSACTION '{gid}'"),
            Self::Deallocate(None) => f.write_str("DEALLOCATE ALL"),
            Self::Deallocate(Some(n)) => write!(f, "DEALLOCATE {}", quote_ident(n)),
            Self::DeclareCursor {
                name,
                scroll,
                hold,
                query,
            } => {
                write!(f, "DECLARE {} ", quote_ident(name))?;
                match scroll {
                    Some(true) => f.write_str("SCROLL ")?,
                    Some(false) => f.write_str("NO SCROLL ")?,
                    None => {}
                }
                f.write_str("CURSOR ")?;
                if *hold {
                    f.write_str("WITH HOLD ")?;
                }
                write!(f, "FOR {query}")
            }
            Self::FetchCursor { name, direction } => {
                write!(f, "FETCH {direction} FROM {}", quote_ident(name))
            }
            Self::MoveCursor { name, direction } => {
                write!(f, "MOVE {direction} FROM {}", quote_ident(name))
            }
            Self::CloseCursor { name } => match name {
                Some(n) => write!(f, "CLOSE {}", quote_ident(n)),
                None => f.write_str("CLOSE ALL"),
            },
            Self::Listen(ch) => write!(f, "LISTEN {}", quote_ident(ch)),
            Self::Notify { channel, payload } => {
                write!(f, "NOTIFY {}", quote_ident(channel))?;
                if let Some(p) = payload {
                    write!(f, ", '{}'", p.replace('\'', "''"))?;
                }
                Ok(())
            }
            Self::Unlisten(ch) => match ch {
                Some(c) => write!(f, "UNLISTEN {}", quote_ident(c)),
                None => f.write_str("UNLISTEN *"),
            },
            Self::CopyTo {
                table,
                columns,
                query,
                options,
            } => {
                if let Some(q) = query {
                    write!(f, "COPY ({q})")?;
                } else {
                    write!(f, "COPY {table}")?;
                    if let Some(cols) = columns {
                        write!(f, " ({})", cols.join(", "))?;
                    }
                }
                write!(f, " TO STDOUT")?;
                let mut parts: Vec<String> = Vec::new();
                if options.format == CopyFormat::Csv {
                    parts.push("FORMAT csv".to_string());
                }
                if options.header {
                    parts.push("HEADER true".to_string());
                }
                if let Some(d) = options.delimiter {
                    parts.push(alloc::format!("DELIMITER '{d}'"));
                }
                if let Some(n) = &options.null_str {
                    parts.push(alloc::format!("NULL '{n}'"));
                }
                if let Some(q) = options.quote {
                    parts.push(alloc::format!("QUOTE '{q}'"));
                }
                if !parts.is_empty() {
                    write!(f, " WITH ({})", parts.join(", "))?;
                }
                Ok(())
            }
            Self::CopyFromFile {
                table,
                columns,
                path,
                options,
            } => {
                write!(f, "COPY {table}")?;
                if let Some(cols) = columns {
                    write!(f, " ({})", cols.join(", "))?;
                }
                write!(f, " FROM '{path}'")?;
                let mut parts: Vec<String> = Vec::new();
                if options.format == CopyFormat::Csv {
                    parts.push("FORMAT csv".to_string());
                }
                if options.header {
                    parts.push("HEADER true".to_string());
                }
                if let Some(d) = options.delimiter {
                    parts.push(alloc::format!("DELIMITER '{d}'"));
                }
                if let Some(n) = &options.null_str {
                    parts.push(alloc::format!("NULL '{n}'"));
                }
                if let Some(q) = options.quote {
                    parts.push(alloc::format!("QUOTE '{q}'"));
                }
                if !parts.is_empty() {
                    write!(f, " WITH ({})", parts.join(", "))?;
                }
                Ok(())
            }
            Self::CopyToFile {
                table,
                columns,
                query,
                path,
                options,
            } => {
                if let Some(q) = query {
                    write!(f, "COPY ({q})")?;
                } else {
                    write!(f, "COPY {table}")?;
                    if let Some(cols) = columns {
                        write!(f, " ({})", cols.join(", "))?;
                    }
                }
                write!(f, " TO '{path}'")?;
                let mut parts: Vec<String> = Vec::new();
                if options.format == CopyFormat::Csv {
                    parts.push("FORMAT csv".to_string());
                }
                if options.header {
                    parts.push("HEADER true".to_string());
                }
                if let Some(d) = options.delimiter {
                    parts.push(alloc::format!("DELIMITER '{d}'"));
                }
                if let Some(n) = &options.null_str {
                    parts.push(alloc::format!("NULL '{n}'"));
                }
                if let Some(q) = options.quote {
                    parts.push(alloc::format!("QUOTE '{q}'"));
                }
                if !parts.is_empty() {
                    write!(f, " WITH ({})", parts.join(", "))?;
                }
                Ok(())
            }
            Self::AlterDomain { name, action } => {
                write!(f, "ALTER DOMAIN {name} ")?;
                match action {
                    AlterDomainAction::AddConstraint { name: cn, check } => match cn {
                        Some(cn) => write!(f, "ADD CONSTRAINT {cn} CHECK ({check})"),
                        None => write!(f, "ADD CHECK ({check})"),
                    },
                    AlterDomainAction::DropConstraint {
                        name: cn,
                        if_exists,
                    } => {
                        if *if_exists {
                            write!(f, "DROP CONSTRAINT IF EXISTS {cn}")
                        } else {
                            write!(f, "DROP CONSTRAINT {cn}")
                        }
                    }
                    AlterDomainAction::SetDefault(e) => write!(f, "SET DEFAULT {e}"),
                    AlterDomainAction::DropDefault => f.write_str("DROP DEFAULT"),
                    AlterDomainAction::SetNotNull => f.write_str("SET NOT NULL"),
                    AlterDomainAction::DropNotNull => f.write_str("DROP NOT NULL"),
                    AlterDomainAction::RenameTo(n) => write!(f, "RENAME TO {n}"),
                }
            }
            Self::Truncate {
                tables,
                restart_identity,
                cascade,
                only,
            } => {
                f.write_str("TRUNCATE TABLE ")?;
                if *only {
                    f.write_str("ONLY ")?;
                }
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
            Self::DropIndex {
                name,
                if_exists,
                table,
            } => {
                f.write_str("DROP INDEX ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                write!(f, "{}", quote_ident(name))?;
                if let Some(t) = table {
                    write!(f, " ON {}", quote_ident(t))?;
                }
                Ok(())
            }
            Self::Select(s) => s.fmt(f),
            Self::CreateTable(s) => s.fmt(f),
            Self::CreateIndex(s) => s.fmt(f),
            Self::Insert(s) => s.fmt(f),
            Self::Update(s) => s.fmt(f),
            Self::Delete(s) => s.fmt(f),
            Self::Merge(s) => s.fmt(f),
            Self::Vacuum { table, analyze } => {
                f.write_str("VACUUM")?;
                if *analyze {
                    f.write_str(" ANALYZE")?;
                }
                if let Some(t) = table {
                    write!(f, " {}", quote_ident(t))?;
                }
                Ok(())
            }
            Self::Begin(modes) => {
                f.write_str("BEGIN")?;
                if let Some(level) = modes.isolation {
                    write!(f, " ISOLATION LEVEL {level}")?;
                }
                match modes.read_only {
                    Some(true) => f.write_str(" READ ONLY")?,
                    Some(false) => f.write_str(" READ WRITE")?,
                    None => {}
                }
                Ok(())
            }
            Self::Commit => f.write_str("COMMIT"),
            Self::Rollback => f.write_str("ROLLBACK"),
            Self::Savepoint(n) => write!(f, "SAVEPOINT {}", quote_ident(n)),
            Self::RollbackToSavepoint(n) => write!(f, "ROLLBACK TO SAVEPOINT {}", quote_ident(n)),
            Self::ReleaseSavepoint(n) => write!(f, "RELEASE SAVEPOINT {}", quote_ident(n)),
            Self::ShowTables => f.write_str("SHOW TABLES"),
            Self::ShowDatabases => f.write_str("SHOW DATABASES"),
            Self::UseDatabase(n) => write!(f, "USE {}", quote_ident(n)),
            Self::ShowCreateTable(t) => write!(f, "SHOW CREATE TABLE {}", quote_ident(t)),
            Self::ShowIndexes(t) => write!(f, "SHOW INDEXES FROM {}", quote_ident(t)),
            Self::ShowStatus => f.write_str("SHOW STATUS"),
            Self::ShowVariables => f.write_str("SHOW VARIABLES"),
            Self::ShowVariablesLike(p) => {
                write!(f, "SHOW VARIABLES LIKE '{}'", p.replace('\'', "''"))
            }
            Self::ShowProcesslist => f.write_str("SHOW PROCESSLIST"),
            Self::Discard(t) => write!(f, "DISCARD {t}"),
            Self::Kill { query_only, id } => {
                if *query_only {
                    write!(f, "KILL QUERY {id}")
                } else {
                    write!(f, "KILL CONNECTION {id}")
                }
            }
            Self::ShowColumns(t) => write!(f, "SHOW COLUMNS FROM {}", quote_ident(t)),
            Self::CreateUser(s) => write!(
                f,
                "CREATE USER {} WITH PASSWORD '<redacted>' ROLE '{}'",
                quote_ident(&s.name),
                s.role
            ),
            Self::DropUser { name, if_exists } => {
                let ie = if *if_exists { "IF EXISTS " } else { "" };
                write!(f, "DROP USER {ie}{}", quote_ident(name))
            }
            Self::SetRole(Some(r)) => write!(f, "SET ROLE {}", quote_ident(r)),
            Self::SetRole(None) => f.write_str("RESET ROLE"),
            Self::Grant(g) => write!(f, "{}", g.render(true)),
            Self::Revoke(g) => write!(f, "{}", g.render(false)),
            Self::CreatePolicy(s) => {
                write!(
                    f,
                    "CREATE POLICY {} ON {}",
                    quote_ident(&s.name),
                    quote_ident(&s.table)
                )?;
                if !s.permissive {
                    f.write_str(" AS RESTRICTIVE")?;
                }
                if !matches!(s.cmd, PolicyCmd::All) {
                    let w = match s.cmd {
                        PolicyCmd::Select => "SELECT",
                        PolicyCmd::Insert => "INSERT",
                        PolicyCmd::Update => "UPDATE",
                        PolicyCmd::Delete => "DELETE",
                        PolicyCmd::All => unreachable!(),
                    };
                    write!(f, " FOR {w}")?;
                }
                if !s.roles.is_empty() {
                    write!(f, " TO {}", s.roles.join(", "))?;
                }
                if let Some(u) = &s.using {
                    write!(f, " USING ({u})")?;
                }
                if let Some(c) = &s.with_check {
                    write!(f, " WITH CHECK ({c})")?;
                }
                Ok(())
            }
            Self::AlterPolicy(s) => {
                write!(
                    f,
                    "ALTER POLICY {} ON {}",
                    quote_ident(&s.name),
                    quote_ident(&s.table)
                )?;
                if let Some(nn) = &s.rename_to {
                    return write!(f, " RENAME TO {}", quote_ident(nn));
                }
                if let Some(roles) = &s.roles {
                    write!(f, " TO {}", roles.join(", "))?;
                }
                if let Some(u) = &s.using {
                    write!(f, " USING ({u})")?;
                }
                if let Some(c) = &s.with_check {
                    write!(f, " WITH CHECK ({c})")?;
                }
                Ok(())
            }
            Self::DropPolicy(s) => {
                f.write_str("DROP POLICY ")?;
                if s.if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                write!(f, "{} ON {}", quote_ident(&s.name), quote_ident(&s.table))
            }
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
            Self::DropSubscription { name, if_exists } => {
                let opt = if *if_exists { "IF EXISTS " } else { "" };
                write!(f, "DROP SUBSCRIPTION {opt}{}", quote_ident(name))
            }
            Self::WaitForWalPosition { pos, timeout_ms } => {
                write!(f, "WAIT FOR WAL POSITION {pos}")?;
                if let Some(ms) = timeout_ms {
                    write!(f, " WITH TIMEOUT {ms}")?;
                }
                Ok(())
            }
            Self::RenameTables(pairs) => {
                f.write_str("RENAME TABLE ")?;
                for (i, (from, to)) in pairs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} TO {}", quote_ident(from), quote_ident(to))?;
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
                    // Parameters are consumed, not stored; the shortest
                    // faithful spelling.
                    AlterIndexTarget::StorageParams => {
                        write!(f, "{} SET ()", quote_ident(&a.name))
                    }
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
                    PublicationScope::TablesInSchema(schema) => {
                        write!(f, " FOR TABLES IN SCHEMA {}", quote_ident(schema))?;
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
            Self::DropPublication { name, if_exists } => {
                let opt = if *if_exists { "IF EXISTS " } else { "" };
                write!(f, "DROP PUBLICATION {opt}{}", quote_ident(name))
            }
            Self::SetParameter { name, value, local } => {
                write!(f, "SET {}{name} = ", if *local { "LOCAL " } else { "" })?;
                match value {
                    SetValue::String(s) => write!(f, "'{}'", s.replace('\'', "''")),
                    SetValue::Ident(s) | SetValue::Number(s) => f.write_str(s),
                    SetValue::Default => f.write_str("DEFAULT"),
                }
            }
            Self::SetTransaction { modes } => {
                f.write_str("SET TRANSACTION")?;
                if let Some(isolation) = modes.isolation {
                    let name = match isolation {
                        IsolationLevel::ReadUncommitted => "READ UNCOMMITTED",
                        IsolationLevel::ReadCommitted => "READ COMMITTED",
                        IsolationLevel::RepeatableRead => "REPEATABLE READ",
                        IsolationLevel::Serializable => "SERIALIZABLE",
                    };
                    write!(f, " ISOLATION LEVEL {name}")?;
                }
                match modes.read_only {
                    Some(true) => f.write_str(" READ ONLY")?,
                    Some(false) => f.write_str(" READ WRITE")?,
                    None => {}
                }
                Ok(())
            }
            Self::ShowParameter(name) => write!(f, "SHOW {name}"),
            Self::SetUserVars(assigns, _) => {
                f.write_str("SET ")?;
                for (i, (name, value)) in assigns.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "@{name} = {value}")?;
                }
                Ok(())
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
            Self::DropFunction {
                name,
                args,
                if_exists,
            } => {
                f.write_str("DROP FUNCTION ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                write!(f, "{}", quote_ident(name))?;
                if let Some(a) = args {
                    write!(f, "({})", a.join(", "))?;
                }
                Ok(())
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
            Self::CommentOn {
                kind,
                name,
                comment,
            } => {
                let body = match comment {
                    Some(c) => alloc::format!("'{}'", c.replace('\'', "''")),
                    None => "NULL".into(),
                };
                write!(f, "COMMENT ON {} {name} IS {body}", kind.to_uppercase())
            }
            Self::AlterTypeRenameValue {
                type_name,
                old,
                new,
            } => write!(
                f,
                "ALTER TYPE {} RENAME VALUE '{}' TO '{}'",
                quote_ident(type_name),
                old.replace('\'', "''"),
                new.replace('\'', "''")
            ),
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
                    write!(
                        f,
                        " {} '{anchor}'",
                        if *is_before { "BEFORE" } else { "AFTER" }
                    )?;
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
            Self::CreateRule(r) => {
                f.write_str("CREATE ")?;
                if r.or_replace {
                    f.write_str("OR REPLACE ")?;
                }
                write!(
                    f,
                    "RULE {} AS ON {} TO {}",
                    quote_ident(&r.name),
                    r.event,
                    quote_ident(&r.table)
                )?;
                if let Some(w) = &r.when_condition {
                    write!(f, " WHERE {w}")?;
                }
                f.write_str(if r.instead {
                    " DO INSTEAD "
                } else {
                    " DO ALSO "
                })?;
                if r.commands.is_empty() {
                    f.write_str("NOTHING")?;
                } else if r.commands.len() == 1 {
                    write!(f, "{}", r.commands[0])?;
                } else {
                    f.write_str("(")?;
                    for (i, c) in r.commands.iter().enumerate() {
                        if i > 0 {
                            f.write_str("; ")?;
                        }
                        write!(f, "{c}")?;
                    }
                    f.write_str(")")?;
                }
                Ok(())
            }
            Self::DropRule {
                name,
                table,
                if_exists,
            } => {
                f.write_str("DROP RULE ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                write!(f, "{} ON {}", quote_ident(name), quote_ident(table))
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
            TypeKind::Composite { fields, .. } => {
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
        write!(f, " AS {}", self.body)?;
        match self.check_option {
            Some(ViewCheckOption::Local) => f.write_str(" WITH LOCAL CHECK OPTION"),
            Some(ViewCheckOption::Cascaded) => f.write_str(" WITH CASCADED CHECK OPTION"),
            None => Ok(()),
        }
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
        // v7.39 (read01 round 64) — the EXCEPTION section. It was MISSING from
        // this Display, and `CREATE FUNCTION` stores a body by re-rendering the
        // parsed block through it — so every exception handler a function
        // declared was thrown away AT STORE TIME. The block executed fine while
        // it was still an AST (a DO block never round-trips through text), which
        // is why only functions and triggers lost theirs.
        if !self.exception_handlers.is_empty() {
            f.write_str("EXCEPTION\n")?;
            for h in &self.exception_handlers {
                writeln!(f, "  WHEN {} THEN", h.conditions.join(" OR "))?;
                for stmt in &h.body {
                    writeln!(f, "    {stmt};")?;
                }
            }
        }
        f.write_str("END")
    }
}

impl fmt::Display for PlPgSqlStmt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Assign { target, value } => write!(f, "{target} := {value}"),
            Self::SelectInto { var, body } => write!(f, "{body} INTO {var}"),
            Self::ReturnNext(e) => write!(f, "RETURN NEXT {e}"),
            Self::ReturnQuery(s) => write!(f, "RETURN QUERY {s}"),
            Self::ReturnQueryExecute { sql } => write!(f, "RETURN QUERY EXECUTE {sql}"),
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
        AlterTableTarget::OfType { type_name } => write!(f, "OF {type_name}"),
        AlterTableTarget::ReplicaIdentityUsingIndex { index } => {
            write!(f, "REPLICA IDENTITY USING INDEX {index}")
        }
        AlterTableTarget::Inherit { parent, detach } => {
            if *detach {
                write!(f, "NO INHERIT {parent}")
            } else {
                write!(f, "INHERIT {parent}")
            }
        }
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
        AlterTableTarget::DropIndex { name, if_exists } => {
            f.write_str("DROP INDEX ")?;
            if *if_exists {
                f.write_str("IF EXISTS ")?;
            }
            write!(f, "{}", quote_ident(name))
        }
        AlterTableTarget::ModifyColumn {
            column,
            rename_to,
            definition,
            position,
        } => {
            if let Some(new) = rename_to {
                write!(
                    f,
                    "CHANGE COLUMN {} {} {}",
                    quote_ident(column),
                    quote_ident(new),
                    definition.ty
                )?;
            } else {
                write!(f, "MODIFY COLUMN {} {}", quote_ident(column), definition.ty)?;
            }
            if !definition.nullable {
                f.write_str(" NOT NULL")?;
            }
            write_column_position(f, position.as_ref())
        }
        AlterTableTarget::RenameIndex { old, new } => {
            write!(
                f,
                "RENAME INDEX {} TO {}",
                quote_ident(old),
                quote_ident(new)
            )
        }
        AlterTableTarget::SetTableAutoIncrement(n) => write!(f, "AUTO_INCREMENT = {n}"),
        AlterTableTarget::SetEngine(name) => write!(f, "ENGINE = {name}"),
        AlterTableTarget::ConvertToCharacterSet { charset, collate } => {
            write!(f, "CONVERT TO CHARACTER SET {charset}")?;
            if let Some(c) = collate {
                write!(f, " COLLATE {c}")?;
            }
            Ok(())
        }
        AlterTableTarget::AddColumn {
            column,
            if_not_exists,
            position,
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
            collation,
        } => {
            write!(f, "ALTER COLUMN {} TYPE {new_type}", quote_ident(column))?;
            if let Some((_, name)) = collation {
                write!(f, " COLLATE {}", quote_ident(name))?;
            }
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
        AlterTableTarget::ValidateConstraint { name } => {
            write!(f, "VALIDATE CONSTRAINT {}", quote_ident(name))
        }
        AlterTableTarget::OwnerTo { role } => write!(f, "OWNER TO {}", quote_ident(role)),
        AlterTableTarget::ClusterOn { index } => match index {
            Some(i) => write!(f, "CLUSTER ON {}", quote_ident(i)),
            None => f.write_str("SET WITHOUT CLUSTER"),
        },
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
        AlterTableTarget::RenameConstraint { old, new } => {
            write!(
                f,
                "RENAME CONSTRAINT {} TO {}",
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
        AlterTableTarget::SetRowSecurity { enabled, force } => match (enabled, force) {
            (Some(true), _) => f.write_str("ENABLE ROW LEVEL SECURITY"),
            (Some(false), _) => f.write_str("DISABLE ROW LEVEL SECURITY"),
            (_, Some(true)) => f.write_str("FORCE ROW LEVEL SECURITY"),
            (_, Some(false)) => f.write_str("NO FORCE ROW LEVEL SECURITY"),
            (None, None) => Ok(()),
        },
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
        AlterTableTarget::AlterColumnRestart { column, with } => {
            write!(f, "ALTER COLUMN {} RESTART", quote_ident(column))?;
            if let Some(n) = with {
                write!(f, " WITH {n}")?;
            }
            Ok(())
        }
        AlterTableTarget::AlterColumnDropExpression { column, if_exists } => {
            write!(
                f,
                "ALTER COLUMN {} DROP EXPRESSION{}",
                quote_ident(column),
                if *if_exists { " IF EXISTS" } else { "" }
            )
        }
        AlterTableTarget::AlterColumnDropIdentity { column, if_exists } => {
            write!(
                f,
                "ALTER COLUMN {} DROP IDENTITY{}",
                quote_ident(column),
                if *if_exists { " IF EXISTS" } else { "" }
            )
        }
        AlterTableTarget::AlterColumnSetExpression { column, expr } => {
            write!(
                f,
                "ALTER COLUMN {} SET EXPRESSION AS ({expr})",
                quote_ident(column)
            )
        }
    }
}

impl fmt::Display for TableConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrimaryKey { name, columns, .. } => {
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
                ..
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
            Self::Check {
                name,
                expr,
                not_valid,
            } => {
                if let Some(n) = name {
                    write!(f, "CONSTRAINT {} ", quote_ident(n))?;
                }
                write!(f, "CHECK ({expr})")?;
                if *not_valid {
                    write!(f, " NOT VALID")?;
                }
                Ok(())
            }
            Self::Index {
                name,
                columns,
                prefix_lengths,
            } => {
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
                    // v7.40.0 — the declared prefix rounds back with it.
                    if let Some(Some(p)) = prefix_lengths.get(i) {
                        write!(f, "({p})")?;
                    }
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
            Self::Exclude {
                name,
                method,
                elements,
            } => {
                if let Some(n) = name {
                    write!(f, "CONSTRAINT {} ", quote_ident(n))?;
                }
                f.write_str("EXCLUDE ")?;
                if let Some(m) = method {
                    write!(f, "USING {m} ")?;
                }
                f.write_str("(")?;
                for (i, (col, op)) in elements.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} WITH {op}", quote_ident(col))?;
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
        if let Some(w) = &self.index_where {
            write!(f, " WHERE {w}")?;
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
        // v7.39 (round 413) — MySQL `UPDATE … ORDER BY … LIMIT n`.
        if let Some(ol) = self.order_limit.as_deref() {
            if !ol.order_by.is_empty() {
                f.write_str(" ORDER BY ")?;
                for (i, o) in ol.order_by.iter().enumerate() {
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
            if let Some(n) = ol.limit {
                write!(f, " LIMIT {n}")?;
            }
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
            Self::Merge(s) => write!(f, "{s}"),
        }
    }
}

impl fmt::Display for MergeStatement {
    // v7.17.0 Phase 3.P0-42 — MERGE display is approximate
    // (it round-trips for the cases tests cover, not for
    // round-tripping every edge of the surface).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_with_clause(&self.ctes, f)?;
        f.write_str("MERGE INTO ")?;
        write!(f, "{}", quote_ident(&self.target))?;
        if let Some(a) = &self.target_alias {
            write!(f, " {}", quote_ident(a))?;
        }
        f.write_str(" USING ")?;
        if let Some(sub) = &self.source_select {
            write!(f, "({sub})")?;
        } else {
            write!(f, "{}", quote_ident(&self.source))?;
        }
        if let Some(a) = &self.source_alias {
            write!(f, " {}", quote_ident(a))?;
        }
        if !self.source_column_aliases.is_empty() {
            f.write_str("(")?;
            for (i, c) in self.source_column_aliases.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", quote_ident(c))?;
            }
            f.write_str(")")?;
        }
        write!(f, " ON {}", self.on)?;
        for clause in &self.clauses {
            f.write_str(" WHEN ")?;
            f.write_str(match clause.matched {
                MergeMatched::Matched => "MATCHED",
                MergeMatched::NotMatched => "NOT MATCHED",
                MergeMatched::NotMatchedBySource => "NOT MATCHED BY SOURCE",
            })?;
            if let Some(c) = &clause.condition {
                write!(f, " AND {c}")?;
            }
            f.write_str(" THEN ")?;
            match &clause.action {
                MergeAction::Insert { columns, values } => {
                    f.write_str("INSERT ")?;
                    // A column list is optional (round 146): the bare
                    // `INSERT VALUES (…)` form maps positionally.
                    if !columns.is_empty() {
                        f.write_str("(")?;
                        for (i, c) in columns.iter().enumerate() {
                            if i > 0 {
                                f.write_str(", ")?;
                            }
                            write!(f, "{}", quote_ident(c))?;
                        }
                        f.write_str(") ")?;
                    }
                    f.write_str("VALUES (")?;
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
        if let Some(items) = &self.returning {
            f.write_str(" RETURNING ")?;
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{it}")?;
            }
        }
        Ok(())
    }
}

/// Shared `WITH <cte> [, …] ` prefix renderer — SELECT and MERGE both
/// carry a CTE list and must round-trip it identically.
fn fmt_with_clause(ctes: &[Cte], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if ctes.is_empty() {
        return Ok(());
    }
    f.write_str("WITH ")?;
    if ctes.iter().any(|c| c.recursive) {
        f.write_str("RECURSIVE ")?;
    }
    for (i, cte) in ctes.iter().enumerate() {
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
    f.write_str(" ")
}

impl fmt::Display for SelectStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // v7.30.1 (mailrs round-24 class audit) — the WITH clause
        // must survive the round trip; a CTE-using statement
        // re-parsed without it references undefined tables.
        fmt_with_clause(&self.ctes, f)?;
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
            Self::QualifiedWildcard(q) => write!(f, "{}.*", quote_ident(q)),
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
                JoinKind::Semi => write!(f, " SEMI JOIN {}", j.table)?,
            }
            if let Some(on) = &j.on {
                write!(f, " ON {on}")?;
            }
        }
        Ok(())
    }
}

/// v7.39 (round 205) — render a JSON_TABLE COLUMNS list (recursive
/// for NESTED). Kept close to the parser's grammar so it re-parses.
fn fmt_json_table_columns(f: &mut fmt::Formatter<'_>, cols: &[JsonTableColumn]) -> fmt::Result {
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        match c {
            JsonTableColumn::Ordinality { name } => {
                write!(f, "{} FOR ORDINALITY", quote_ident(name))?;
            }
            JsonTableColumn::Nested { path, columns } => {
                write!(f, "NESTED PATH '{path}' COLUMNS (")?;
                fmt_json_table_columns(f, columns)?;
                f.write_str(")")?;
            }
            JsonTableColumn::Regular {
                name,
                ty,
                path,
                exists,
                format_json,
                wrapper,
                on_empty,
                on_error,
            } => {
                write!(f, "{} {ty}", quote_ident(name))?;
                if *format_json {
                    f.write_str(" FORMAT JSON")?;
                }
                if *exists {
                    write!(f, " EXISTS PATH '{path}'")?;
                } else {
                    write!(f, " PATH '{path}'")?;
                }
                if *wrapper {
                    f.write_str(" WITH WRAPPER")?;
                }
                if let JsonTableOnBehavior::Error = on_empty {
                    f.write_str(" ERROR ON EMPTY")?;
                } else if let JsonTableOnBehavior::Default(e) = on_empty {
                    write!(f, " DEFAULT {e} ON EMPTY")?;
                }
                if let JsonTableOnBehavior::Error = on_error {
                    f.write_str(" ERROR ON ERROR")?;
                } else if let JsonTableOnBehavior::Default(e) = on_error {
                    write!(f, " DEFAULT {e} ON ERROR")?;
                }
            }
        }
    }
    Ok(())
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // v7.30.1 (mailrs round-24 class audit) — the dynamic
        // table-ref shapes must round-trip: rendering only the
        // (synthetic) name turned LATERAL / unnest() /
        // generate_series() into references to nonexistent tables
        // on re-parse.
        // v7.39 (round 205) — JSON_TABLE round-trips through Display
        // (view bodies, WAL replay of `INSERT … SELECT FROM JSON_TABLE`).
        if let Some(jt) = &self.json_table {
            write!(f, "JSON_TABLE({}, '{}'", jt.doc, jt.row_path)?;
            if !jt.passing.is_empty() {
                f.write_str(" PASSING ")?;
                for (i, (n, e)) in jt.passing.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{e} AS {}", quote_ident(n))?;
                }
            }
            f.write_str(" COLUMNS (")?;
            fmt_json_table_columns(f, &jt.columns)?;
            f.write_str(")")?;
            if let Some(a) = &self.alias {
                write!(f, " AS {}", quote_ident(a))?;
            }
            return Ok(());
        }
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
        // 7.38.1 S5.1 — a FROM-position table function must re-render
        // as the CALL, not its bare name: ARRAY(subquery) desugars by
        // re-parsing the subquery's canonical text, and a dropped
        // argument list turned `pg_options_to_table(x)` into a
        // relation lookup that does not exist.
        if let Some(call) = &self.table_fn_call {
            let (fn_name, args) = call.as_ref();
            write!(f, "{fn_name}(")?;
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{a}")?;
            }
            f.write_str(")")?;
            if let Some(a) = &self.alias {
                write!(f, " AS {}", quote_ident(a))?;
                if !self.unnest_column_aliases.is_empty() {
                    f.write_str("(")?;
                    for (i, c) in self.unnest_column_aliases.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{}", quote_ident(c))?;
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

/// v7.39 (round 311) — render the left spine of an AND / OR chain
/// without re-parenthesising each step, so `((a AND b) AND c)` comes out
/// as `(a) AND (b) AND (c)` the way PG's deparse writes it. Only the
/// SAME operator flattens; anything else is an ordinary operand.
fn write_bool_chain(f: &mut fmt::Formatter<'_>, e: &Expr, op: BinOp) -> fmt::Result {
    if let Expr::Binary {
        lhs,
        op: inner,
        rhs,
    } = e
        && *inner == op
    {
        write_bool_chain(f, lhs, op)?;
        return write!(f, " {op} {rhs}");
    }
    write!(f, "{e}")
}

/// v7.39 (round 311, V32) — PG's PRETTY deparse of an expression, the
/// form `pg_get_constraintdef(oid, true)` and friends return.
///
/// The default [`fmt::Display`] parenthesises every operator node, which
/// is what PG's non-pretty deparse does and what makes the text
/// round-trip. Pretty drops the pairs the grammar can put back, and the
/// rule is NOT plain precedence minimisation — measured against PG 18.4
/// across 37 shapes:
///
///   * the boolean layer follows precedence (NOT > AND > OR): an OR
///     under an AND keeps its parens, an AND under an OR does not, and a
///     comparison under any of them does not (`NOT a > 1`);
///   * an associative chain flattens completely, even where the source
///     nested it to the right (`a AND (b AND c)` prints as one chain);
///   * but an operand of a comparison or arithmetic operator keeps its
///     parens whenever it is itself an operator expression — so
///     `(a + b) > 10` and `(- a) + b`, even though precedence alone
///     would not require either. A cast, function call, column or
///     literal in that position does not (`a::text = t`,
///     `length(code) > 2`); a cast counts as compound exactly when the
///     thing it casts is (`((a + b)::text) = t`).
///
/// Anything outside that layer defers to `Display`, which is never
/// wrong — only more parenthesised than PG would print.
#[must_use]
pub fn pretty_expr(e: &Expr) -> String {
    let mut out = String::new();
    write_pretty(&mut out, e, PrettyParent::None, false, false);
    out
}

/// v7.39 (round 527) — the same deparse, spelling a cast the way MySQL
/// writes it.
///
/// MariaDB names the offending expression in its out-of-range message
/// and quotes the user's own syntax: `cast(1 as unsigned) - 2`. SPG
/// answered `1::unsigned - 2` — PG's spelling, in a message going to a
/// MySQL client, for a cast the client had just written the other way.
#[must_use]
pub fn pretty_expr_mysql(e: &Expr) -> String {
    let mut out = String::new();
    write_pretty(&mut out, e, PrettyParent::None, false, true);
    out
}

/// v7.39 (round 505) — how strongly an expression suggests its own column
/// name. A cast keeps its argument's name only when that name is STRONG;
/// otherwise the cast reports the type it casts to.
///
/// Deduced from measurement, not from a rulebook. `upper(s)::text` names
/// itself `upper` on PG18 but `(CASE WHEN a=1 THEN 1 END)::text` names
/// itself `text` — so `case` and a function name cannot be the same kind of
/// answer, even though a bare `CASE …` does report `case`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NameStrength {
    /// Nothing to go on — PG reports `?column?`.
    None,
    /// A name, but one a cast overrides: `case`, or a type name.
    Weak,
    /// A name a cast keeps: a column, or the function that produced it.
    Strong,
}

/// v7.39 (round 505) — the column name PG18 gives a projected expression
/// that carries no `AS` alias. `None` means `?column?`.
///
/// SPG used to print the parsed expression back out, which matched neither
/// oracle and made name-keyed row access miss on both wires:
///
/// | query        | PG18       | SPG (before) |
/// |--------------|------------|--------------|
/// | `upper(s)`   | `upper`    | `upper(s)`   |
/// | `a+b`        | `?column?` | `(a + b)`    |
/// | `'lit'`      | `?column?` | `'lit'`      |
/// | `CASE …`     | `case`     | `CASE WHEN (a = 1) THEN …` |
///
/// Every rule below is one of those measurements, taken with `\gdesc`
/// against PG18: a call is named for its function, a cast recurses into its
/// argument and falls back to the type, a scalar subquery takes the name of
/// the column it selects, and operators have no name at all.
#[must_use]
pub fn figure_column_name(expr: &Expr) -> Option<String> {
    let (name, _) = figure_name_inner(expr);
    name
}

/// The name a function reports, which is not always the name SPG parsed it
/// under: `count(*)` is held as `count_star` so the star arity survives the
/// AST, and that internal spelling must not reach a client. PG18 reports
/// `count`.
/// v7.39.13 — public, because Describe was naming the same call from a
/// second map that did not have this entry.
///
/// `count(*)` is held as `count_star` so the star arity survives the
/// AST. The projection mapped it back and the extended protocol's
/// Describe did not, so `SELECT count(*) OVER ()` answered `count` in
/// the row stream and `count_star` to `\gdesc` — an ORM-visible column
/// name, and two answers to one question. Reported by sentori against
/// 7.39.12.
#[must_use]
pub fn canonical_function_name(name: &str) -> String {
    match name {
        "count_star" => "count".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// v7.38.7 — a cast target's `pg_type.typname`, for the name a cast
/// reports when its operand has none of its own. Only the spellings that
/// differ from what the user writes need an entry; everything else is
/// already its own typname.
fn cast_target_typname(target: &CastTarget) -> String {
    let written = target.to_string().to_ascii_lowercase();
    let base = written.strip_suffix("[]").unwrap_or(&written);
    let mapped = match base {
        "bigint" => "int8",
        "integer" | "int" => "int4",
        "smallint" => "int2",
        "boolean" => "bool",
        "double precision" => "float8",
        "real" => "float4",
        "character varying" => "varchar",
        "character" => "bpchar",
        "timestamp with time zone" => "timestamptz",
        "timestamp without time zone" => "timestamp",
        "time without time zone" => "time",
        "decimal" => "numeric",
        other => other,
    };
    if written.ends_with("[]") {
        alloc::format!("_{mapped}")
    } else {
        String::from(mapped)
    }
}

fn figure_name_inner(expr: &Expr) -> (Option<String>, NameStrength) {
    let strong = |n: String| (Some(n), NameStrength::Strong);
    match expr {
        // A column keeps its own name, qualifier and all discarded:
        // `lbl.a` reports `a`.
        Expr::Column(c) => strong(c.name.clone()),
        // Calls are named for the function. This covers the shapes that
        // only LOOK like syntax — `EXTRACT(year FROM …)` reports
        // `extract`, `SUBSTRING(x FROM 1 FOR 2)` reports `substring` —
        // because PG resolves them to functions before naming them.
        Expr::FunctionCall { name, .. } | Expr::WindowFunction { name, .. } => {
            strong(canonical_function_name(name))
        }
        Expr::AggregateOrdered { call, .. } => figure_name_inner(call),
        Expr::Extract { .. } => strong("extract".to_string()),
        Expr::Exists { .. } => strong("exists".to_string()),
        Expr::Array(_) => strong("array".to_string()),
        // `(expr).field` is named for the field, as a column would be.
        Expr::FieldAccess { field, .. } => strong(field.clone()),
        // v7.39.12 — PostgreSQL names a subscript after its operand, so
        // `arr[1]` is `arr`. There was no arm, so it fell through to
        // `?column?`. Reported by sentori against 7.39.11 — the same
        // naming defect v7.38.20 closed, reached through a different
        // expression. Weak, like the field access above it: an outer
        // cast or function still names the column.
        Expr::ArraySubscript { target, .. } => (figure_name_inner(target).0, NameStrength::Weak),
        // A cast prefers its argument's name and settles for the type:
        // `upper(s)::text` is `upper`, `(a+b)::text` is `text`.
        Expr::Cast {
            expr: inner,
            target,
        } => match figure_name_inner(inner) {
            (Some(n), NameStrength::Strong) => strong(n),
            // v7.38.7 — the fallback is the target type's INTERNAL name,
            // which is what PG reports: `SELECT 7::bigint` is `int8`, not
            // the `bigint` the user typed. Measured on PG18 alongside
            // `CAST(7 AS bigint)`, which answers `int8` too.
            _ => (Some(cast_target_typname(target)), NameStrength::Weak),
        },
        // A scalar subquery reports whatever its single output column
        // reports: `(SELECT max(b) …)` is `max`, `(SELECT a+b …)` is not.
        Expr::ScalarSubquery(sel) => scalar_subquery_name(sel),
        // `CASE …` names itself, but weakly — a cast around it wins.
        Expr::Case { .. } => (Some("case".to_string()), NameStrength::Weak),
        // A literal that carries its own type names itself for that type:
        // `INTERVAL '1 day'` reports `interval`, while a bare `'1 day'`
        // reports nothing. Weak, like any other type name.
        Expr::Literal(Literal::Interval { .. }) => {
            (Some("interval".to_string()), NameStrength::Weak)
        }
        // A wrapper that adds no name of its own.
        Expr::Variadic(inner) => figure_name_inner(inner),
        Expr::NamedArg { expr: inner, .. } => figure_name_inner(inner),
        // Everything else — operators, comparisons, IS NULL, LIKE, IN,
        // literals, placeholders — reports `?column?`.
        _ => (None, NameStrength::None),
    }
}

/// The name a scalar subquery's single projected column reports.
fn scalar_subquery_name(sel: &SelectStatement) -> (Option<String>, NameStrength) {
    match sel.items.as_slice() {
        [SelectItem::Expr { alias: Some(a), .. }] => (Some(a.clone()), NameStrength::Strong),
        [SelectItem::Expr { expr, alias: None }] => figure_name_inner(expr),
        _ => (None, NameStrength::None),
    }
}

/// Binding power. Higher binds tighter; 0 means "no enclosing operator".
fn pretty_prec(e: &Expr) -> u8 {
    match e {
        Expr::Binary { op, .. } => match op {
            // v7.39 (round 407) — this deparse ladder mirrors the parser's:
            // OR < XOR < AND < NOT < comparison < additive < multiplicative.
            // XOR (MySQL-only) sits between OR and AND, so AND and everything
            // above shifted +1 to open rung 2 for it.
            BinOp::Or => 1,
            BinOp::LogicalXor => 2,
            BinOp::And => 3,
            BinOp::Add | BinOp::Sub | BinOp::Concat => 6,
            BinOp::Mul | BinOp::Div | BinOp::Mod => 7,
            // Everything else in this enum is a comparison-shaped
            // operator; they share one level, as in the grammar.
            _ => 5,
        },
        Expr::Unary { op, .. } => match op {
            UnOp::Not => 4,
            UnOp::Neg | UnOp::BitNot | UnOp::Plus => 8,
        },
        _ => u8::MAX,
    }
}

/// Is this node an operator expression — the thing an arithmetic or
/// comparison parent keeps parentheses around? A cast inherits the
/// answer from what it casts.
fn pretty_is_compound(e: &Expr) -> bool {
    match e {
        Expr::Binary { .. } | Expr::Unary { .. } => true,
        Expr::Cast { expr, .. } => pretty_is_compound(expr),
        _ => false,
    }
}

/// `parent` describes the enclosing operator: its binding power, and
/// whether it is a comparison (which keeps parens around any operator
/// operand) or a NOT (which keeps them at equal power too).
#[derive(Clone, Copy, PartialEq)]
enum PrettyParent {
    /// Nothing encloses this node.
    None,
    /// A comparison-shaped operator: an operator operand always keeps
    /// its parens, whatever precedence would allow.
    Comparison,
    /// Arithmetic / concatenation: precedence decides.
    Arith(u8),
    /// A boolean connective: precedence decides.
    Bool(u8),
    /// `NOT`: precedence decides, but equal power still needs parens so
    /// `NOT (NOT a > 1)` does not collapse.
    Not,
}

fn write_pretty(out: &mut String, e: &Expr, parent: PrettyParent, is_rhs: bool, mysql: bool) {
    let prec = pretty_prec(e);
    let is_unary_sign = matches!(
        e,
        Expr::Unary {
            op: UnOp::Neg | UnOp::BitNot | UnOp::Plus,
            ..
        }
    );
    let needs = match parent {
        PrettyParent::None => false,
        PrettyParent::Comparison => pretty_is_compound(e),
        // A sign always keeps its parens under an operator — PG writes
        // `(- a) + b` even though precedence would not require it.
        PrettyParent::Arith(p) => {
            is_unary_sign
                || (matches!(e, Expr::Binary { .. } | Expr::Unary { .. })
                    && (prec < p || (prec == p && is_rhs)))
        }
        PrettyParent::Bool(p) => matches!(e, Expr::Binary { .. } | Expr::Unary { .. }) && prec < p,
        PrettyParent::Not => {
            matches!(e, Expr::Binary { .. } | Expr::Unary { .. }) && prec <= pretty_prec_not()
        }
    };
    if needs {
        out.push('(');
    }
    match e {
        Expr::Binary { lhs, op, rhs } => {
            let child = match op {
                BinOp::And | BinOp::Or => PrettyParent::Bool(prec),
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Concat => {
                    PrettyParent::Arith(prec)
                }
                _ => PrettyParent::Comparison,
            };
            write_pretty(out, lhs, child, false, mysql);
            out.push(' ');
            out.push_str(&alloc::format!("{op}"));
            out.push(' ');
            // AND / OR are associative, so an explicitly right-nested
            // chain still prints as one chain.
            let rhs_is_rhs = !matches!(op, BinOp::And | BinOp::Or);
            write_pretty(out, rhs, child, rhs_is_rhs, mysql);
        }
        Expr::Unary { op, expr } => match op {
            UnOp::Not => {
                out.push_str("NOT ");
                write_pretty(out, expr, PrettyParent::Not, false, mysql);
            }
            UnOp::Neg => {
                out.push_str("- ");
                write_pretty(out, expr, PrettyParent::Comparison, false, mysql);
            }
            UnOp::Plus => {
                out.push_str("+ ");
                write_pretty(out, expr, PrettyParent::Comparison, false, mysql);
            }
            UnOp::BitNot => {
                out.push('~');
                write_pretty(out, expr, PrettyParent::Comparison, false, mysql);
            }
        },
        Expr::Cast { expr, target } => {
            if mysql {
                // MySQL's own spelling, which is what its error messages
                // quote back.
                out.push_str("cast(");
                write_pretty(out, expr, PrettyParent::None, false, mysql);
                out.push_str(&alloc::format!(
                    " as {})",
                    target.to_string().to_lowercase()
                ));
            } else {
                write_pretty(out, expr, PrettyParent::Comparison, false, mysql);
                out.push_str(&alloc::format!("::{target}"));
            }
        }
        Expr::IsNull { expr, negated } => {
            write_pretty(out, expr, PrettyParent::Comparison, false, mysql);
            out.push_str(if *negated { " IS NOT NULL" } else { " IS NULL" });
        }
        other => out.push_str(&alloc::format!("{other}")),
    }
    if needs {
        out.push(')');
    }
}

const fn pretty_prec_not() -> u8 {
    // Must match `pretty_prec`'s `UnOp::Not` rung (v7.39 round 407: 3 → 4
    // when the XOR insertion shifted the deparse ladder up by one).
    4
}

impl fmt::Display for Expr {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(l) => write!(f, "{l}"),
            Self::Column(c) => write!(f, "{c}"),
            Self::Placeholder(n) => write!(f, "${n}"),
            // Round-trips as the spelling PG's docs lead with.
            Self::NamedArg { name, expr } => write!(f, "{} := {expr}", quote_ident(name)),
            Self::Variadic(expr) => write!(f, "VARIADIC {expr}"),
            // Round-trips with the name quoted, which is how PG spells a
            // collation everywhere: `"en_US.utf8"`, `"C"`.
            Self::Collate { expr, collation } => {
                write!(f, "{expr} COLLATE {}", quote_ident(collation))
            }
            // v7.39 (round 311) — an AND / OR chain that nests to the
            // LEFT is one chain, and renders flat: `(a) AND (b) AND (c)`,
            // not `((a) AND (b)) AND (c)`. Explicit right nesting keeps
            // its parentheses, because that is a different grouping as
            // written. Both halves measured against PG 18.4's deparse,
            // which flattens a same-operator left chain at parse time and
            // leaves `a AND (b AND c)` alone.
            Self::Binary { lhs, op, rhs } if matches!(op, BinOp::And | BinOp::Or) => {
                f.write_str("(")?;
                write_bool_chain(f, lhs, *op)?;
                write!(f, " {op} {rhs}")?;
                f.write_str(")")
            }
            Self::Binary { lhs, op, rhs } => write!(f, "({lhs} {op} {rhs})"),
            Self::Unary { op, expr } => match op {
                UnOp::Not => write!(f, "(NOT {expr})"),
                // A space after the sign, as PG's deparse writes it.
                UnOp::Neg => write!(f, "(- {expr})"),
                UnOp::Plus => write!(f, "(+ {expr})"),
                UnOp::BitNot => write!(f, "(~{expr})"),
            },
            // The OPERAND carries the parentheses, not the cast:
            // `(a)::text`, `((a + b))::text`. PG words it this way, and
            // it is what keeps `a::text = t` from reading as a cast of
            // the comparison.
            Self::Cast { expr, target } => write!(f, "({expr})::{target}"),
            Self::FieldAccess { base, field } => write!(f, "({base}).{field}"),
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
            Self::BoolTest {
                expr,
                value,
                negated,
            } => {
                let word = match value {
                    Some(true) => "TRUE",
                    Some(false) => "FALSE",
                    None => "UNKNOWN",
                };
                if *negated {
                    write!(f, "({expr} IS NOT {word})")
                } else {
                    write!(f, "({expr} IS {word})")
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
            Self::RowInSubquery {
                row,
                subquery,
                negated,
            } => {
                write!(f, "(")?;
                for (i, e) in row.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{e}")?;
                }
                let kw = if *negated { ") NOT IN (" } else { ") IN (" };
                write!(f, "{kw}{subquery})")
            }
            Self::RowCmpSubquery { row, op, subquery } => {
                write!(f, "(")?;
                for (i, e) in row.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{e}")?;
                }
                write!(f, ") {op} ({subquery})")
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

/// Render an exact decimal `unscaled / 10^scale`, keeping the scale
/// (trailing zeros): `(200, 2)` → `2.00`, `(1, 1)` → `0.1`, `(-15, 1)` → `-1.5`.
pub fn render_exact_decimal(unscaled: i128, scale: u16) -> alloc::string::String {
    use alloc::string::ToString;
    if scale == 0 {
        return alloc::format!("{unscaled}");
    }
    let neg = unscaled < 0;
    let digits = alloc::format!("{}", unscaled.unsigned_abs());
    let scale = scale as usize;
    let (int_part, frac_part) = if digits.len() > scale {
        (
            digits[..digits.len() - scale].to_string(),
            digits[digits.len() - scale..].to_string(),
        )
    } else {
        ("0".to_string(), alloc::format!("{digits:0>scale$}"))
    };
    alloc::format!("{}{int_part}.{frac_part}", if neg { "-" } else { "" })
}

/// A single-quoted SQL string, with an embedded quote doubled.
fn write_quoted(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
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
            Self::Numeric { unscaled, scale } => {
                // Render the exact decimal `unscaled / 10^scale`, preserving
                // scale (trailing zeros) — round-trips to the same literal.
                f.write_str(&render_exact_decimal(*unscaled, *scale))
            }
            Self::NumericBig(s) => f.write_str(s),
            // Printed exactly as the text form was, so a reader cannot
            // tell whether the constant was decoded or not.
            Self::Timestamp { text, .. } | Self::Date { text, .. } => write_quoted(f, text),
            Self::String(s) => write_quoted(f, s),
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
            Self::IntDiv => "DIV",
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
            Self::GeomParallel => "?||",
            Self::OverLeft => "&<",
            Self::OverRight => "&>",
            Self::GeomPerp => "?-|",
            Self::GeomSameAs => "~=",
            Self::ClosestPoint => "##",
            Self::GeomHoriz => "?-",
            Self::InnerProduct => "<#>",
            Self::CosineDistance => "<=>",
            Self::Concat => "||",
            Self::BitOr => "|",
            Self::BitAnd => "&",
            Self::BitXor => "#",
            Self::LogicalXor => "xor",
            Self::JsonGet => "->",
            Self::JsonGetText => "->>",
            Self::JsonGetPath => "#>",
            Self::JsonGetPathText => "#>>",
            Self::JsonContains => "@>",
            Self::JsonPathExists => "@?",
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
            Self::Intersects => "?#",
            Self::IsBelow => "<^",
            Self::IsAbove => ">^",
            Self::PatternLt => "~<~",
            Self::PatternLtEq => "~<=~",
            Self::PatternGt => "~>~",
            Self::PatternGtEq => "~>=~",
        })
    }
}

/// Quote `s` as a PG double-quoted identifier when required (keyword,
/// non-folded case, leading digit, embedded non-`[A-Za-z0-9_]`, empty).
/// Otherwise return it as-is. Returns an owned `String` to keep the call site
/// uniform.
pub(crate) fn quote_ident(s: &str) -> String {
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
            locking: None,
            items: vec![SelectItem::Wildcard],
            from: Some(FromClause {
                primary: TableRef {
                    name: "users".into(),
                    alias: None,
                    only: false,
                    as_of_segment: None,
                    unnest_expr: None,
                    unnest_column_aliases: Vec::new(),
                    with_ordinality: false,
                    generate_series_args: None,
                    lateral_subquery: None,
                    jsonb_each_text_arg: None,
                    table_fn_call: None,
                    rows_from: None,
                    json_table: None,
                    scalar_fn_item: false,
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
            window_check_exprs: Vec::new(),
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
