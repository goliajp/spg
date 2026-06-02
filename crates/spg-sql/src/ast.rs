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
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainStatement {
    pub analyze: bool,
    pub inner: Box<SelectStatement>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMethod {
    /// Default — B-tree over `IndexKey`. Used for equality / range
    /// lookups on scalar columns.
    BTree,
    /// `USING hnsw` — NSW graph for kNN over a vector column.
    Hnsw,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStatement {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// `IF NOT EXISTS` — engine returns `CommandOk` no-op when the
    /// table name already exists, instead of raising `DuplicateTable`.
    pub if_not_exists: bool,
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
}

/// In-cell encoding for a `VECTOR(N)` column. v6.0.1 added the
/// optional `USING <encoding>` clause; omitting it keeps the
/// pre-v6 `F32` default. `Sq8` quantises each cell to a per-vector
/// affine `(min, max, [u8; dim])` triple (4× compression). `F16`
/// is reserved for v6.0.3 (`USING HALF`).
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
}

impl fmt::Display for VecEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::F32 => f.write_str("F32"),
            Self::Sq8 => f.write_str("SQ8"),
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
    /// v4.9 `JSON` / `JSONB` — text-backed JSON document. No parse-
    /// time validation; the engine round-trips the literal verbatim.
    Json,
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
            },
            Self::Numeric(p, s) => {
                if *s == 0 {
                    write!(f, "NUMERIC({p})")
                } else {
                    write!(f, "NUMERIC({p}, {s})")
                }
            }
            Self::Date => f.write_str("DATE"),
            Self::Timestamp => f.write_str("TIMESTAMP"),
            Self::Json => f.write_str("JSON"),
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
}

/// `DELETE FROM <table> [WHERE cond]`. v4.4 — removes matched rows
/// from the active catalog and prunes them from every index.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub table: String,
    pub where_: Option<Expr>,
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
    pub order_by: Option<OrderBy>,
    pub limit: Option<u32>,
    /// `OFFSET <n>` — drop the first `n` rows after ORDER BY but
    /// before LIMIT (so `LIMIT 10 OFFSET 5` keeps rows 6..=15).
    pub offset: Option<u32>,
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
            Self::Date => "date",
            Self::Timestamp => "timestamp",
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
            Self::Explain(e) => {
                if e.analyze {
                    write!(f, "EXPLAIN ANALYZE {}", e.inner)
                } else {
                    write!(f, "EXPLAIN {}", e.inner)
                }
            }
        }
    }
}

impl fmt::Display for CreateIndexStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE INDEX ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(
            f,
            "{} ON {} ",
            quote_ident(&self.name),
            quote_ident(&self.table)
        )?;
        if matches!(self.method, IndexMethod::Hnsw) {
            f.write_str("USING hnsw ")?;
        }
        write!(f, "({})", quote_ident(&self.column))
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
        f.write_str(")")
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
        if let Some(o) = &self.order_by {
            write!(f, " ORDER BY {}", o.expr)?;
            if o.desc {
                f.write_str(" DESC")?;
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
                },
                joins: vec![],
            }),
            where_: None,
            group_by: None,
            having: None,
            unions: vec![],
            order_by: None,
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
