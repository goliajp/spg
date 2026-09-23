//! The SQLSTATE and client-facing message an [`EngineError`] carries.
//!
//! 8.0.3 — moved here from `spg-server`'s pgwire module, unchanged, so
//! that the engine can answer the question itself. A PL/pgSQL
//! `EXCEPTION WHEN unique_violation` has to know an error's SQLSTATE
//! while the block is still running, which is inside the engine, and a
//! second classifier in the engine would drift from the one the wire
//! uses — the same SQLSTATE for one error, two answers. The server now
//! calls this one.

extern crate alloc;

use alloc::string::{String, ToString};

use crate::EngineError;

/// The internal class prefixes `Display for EngineError` (and `EvalError`) add.
/// Longest first: "eval: type mismatch: " must strip before "eval: ".
fn strip_error_class(msg: &str) -> String {
    const CLASSES: &[&str] = &[
        "eval: type mismatch: ",
        "eval: ",
        "unsupported: ",
        // r184 — lexer errors surface as "parse: lex: …"; the combined
        // prefix must strip in one pass (the list strips only the first
        // match) so psql shows PG's exact message shape. Longest first.
        "parse: lex: ",
        "lex: ",
        "parse: ",
        "storage: ",
    ];
    for c in CLASSES {
        if let Some(rest) = msg.strip_prefix(c) {
            return rest.to_string();
        }
    }
    msg.to_string()
}

/// v7.39 (read01 round 95) — recover PG's 1-based error position for a parse
/// failure so the wire can attach the ErrorResponse `P` field. The position is
/// re-derived from the query text on this cold error path (rather than carried
/// on `ParseError`, which would grow the recursive parse stack). Standard PG
/// string mode (`backslash_escapes = false`) is used — it doesn't shift token
/// offsets in practice. Only syntax errors carry a position today; semantic
/// errors (column-not-found, type mismatch) would need analyzer/eval plumbing
/// and are deferred.
/// v7.39 (read01 round 230) — SQLSTATE for the window-clause errors, or
/// `None` when the message isn't one. PG answers every window-clause
/// complaint with 42P20 WINDOWING_ERROR and reserves 42704
/// UNDEFINED_OBJECT for a genuinely missing window name — a split worth
/// keeping straight, since the copy and redefinition wordings also carry
/// `window "w1"`. Matches on substrings because the engine's `Unsupported`
/// Display prefixes the message.
fn window_sqlstate(msg: &str) -> Option<&'static str> {
    if msg.contains("window functions are not allowed in ")
        || msg.contains("frame start cannot be ")
        || msg.contains("frame end cannot be ")
        || msg.contains("frame starting from ")
        || msg.contains("cannot override PARTITION BY clause of window ")
        || msg.contains("cannot override ORDER BY clause of window ")
        || msg.contains("because it has a frame clause")
        || msg.contains("RANGE with offset PRECEDING/FOLLOWING ")
        || (msg.contains("window \"") && msg.contains("\" is already defined"))
    {
        return Some("42P20");
    }
    if msg.contains("window \"") && msg.contains("\" does not exist") {
        return Some("42704");
    }
    // v7.39 (round 230) — PG implements neither modifier for a windowed
    // call and reports the gap as 0A000 FEATURE_NOT_SUPPORTED.
    if msg.contains("is not implemented for window functions") {
        return Some("0A000");
    }
    None
}

/// v7.39 (round 429) — crate-visible so the MySQL wire can derive its own
/// errno from the SAME classification: the two protocols disagree only on
/// the code's spelling, never on which failure it was.
/// The SQLSTATE PostgreSQL 18.6 gives each COPY refusal. The codes do
/// not follow one rule — FORCE_QUOTE on a COPY FROM is 0A000 while
/// FORCE_NULL on a COPY TO is 22023 — so each is listed as measured.
fn copy_sqlstate(msg: &str) -> Option<&'static str> {
    if msg.contains("missing data for column")
        || msg.contains("extra data after last expected column")
        || msg.contains("in header line")
    {
        return Some("22P04");
    }
    if msg.contains("rows due to data type incompatibility") {
        return Some("22P02");
    }
    if msg.contains("not referenced by COPY") {
        return Some("42P10");
    }
    if msg.contains("cannot be used with COPY TO")
        || msg.contains("requires ON_ERROR to be set to IGNORE")
        || (msg.contains("REJECT_LIMIT (") && msg.contains("must be greater than zero"))
        || (msg.contains("COPY ON_ERROR \"") && msg.contains("not recognized"))
        || (msg.contains("COPY LOG_VERBOSITY \"") && msg.contains("not recognized"))
        || (msg.contains("COPY format \"") && msg.contains("not recognized"))
    {
        return Some("22023");
    }
    if msg.contains("cannot be used with COPY FROM")
        || msg.contains("with HEADER in COPY TO")
        || msg.contains("COPY format \"binary\" is not supported")
    {
        return Some("0A000");
    }
    None
}

pub fn error_to_wire(e: &EngineError) -> (alloc::borrow::Cow<'static, str>, String) {
    // 9.0.0 — a positioned error is classified by what it wraps; the
    // position rides its own wire field.
    if let EngineError::At { inner, .. } = e {
        return error_to_wire(inner);
    }
    // 8.0.3 — a PL/pgSQL error already knows its code.
    if let EngineError::Raised { sqlstate, message } = e {
        return (sqlstate.clone(), message.clone());
    }
    if let EngineError::Cancelled = e {
        return (
            alloc::borrow::Cow::Borrowed("57014"),
            "canceling statement due to statement timeout".to_string(),
        );
    }
    // v7.38 (read01 P3.26) — an aborted-transaction rejection carries PG's
    // 25P02 so clients recognise "commands ignored until end of block".
    if let EngineError::InFailedTransaction = e {
        return (alloc::borrow::Cow::Borrowed("25P02"), e.to_string());
    }
    // v7.38 (read01 P4.02) — a single-row subquery that returned many rows
    // is PG's 21000 CARDINALITY_VIOLATION.
    if let EngineError::CardinalityViolation = e {
        return (alloc::borrow::Cow::Borrowed("21000"), e.to_string());
    }
    // v7.37.17 (Phase E3) — a RR/SER commit that hit a write-write
    // conflict is PG's 40001 SERIALIZATION_FAILURE (clients retry).
    if let EngineError::SerializationFailure(_) = e {
        return (alloc::borrow::Cow::Borrowed("40001"), e.to_string());
    }
    // v7.39 (read01 round 232) — the ORDER BY legality rules are PG's
    // 42P10 INVALID_COLUMN_REFERENCE, and a set-operation arity mismatch is
    // 42601. Ahead of the variant short-circuits for the same reason the
    // window arm below is: these arrive as `Unsupported`, whose Display
    // prefixes the message.
    // v7.39 (round 299, E3 Phase 2) — a wait that ran out of
    // `lock_timeout` is PG's 55P03, same class as NOWAIT; a wait-for
    // cycle is 40P01, which clients retry.
    if let EngineError::LockDeadlock = e {
        return (alloc::borrow::Cow::Borrowed("40P01"), e.to_string());
    }
    {
        let msg = e.to_string();
        if msg.contains("canceling statement due to lock timeout") {
            return (alloc::borrow::Cow::Borrowed("55P03"), msg);
        }
    }
    // v7.39 (round 297, E3 Phase 1b) — `FOR UPDATE NOWAIT` on a row
    // another transaction holds is PG's 55P03 LOCK_NOT_AVAILABLE.
    // Clients catch that code specifically to back off and retry, so
    // reporting the generic 42000 would be caught by nothing.
    {
        let msg = e.to_string();
        if msg.contains("could not obtain lock on row in relation") {
            return (alloc::borrow::Cow::Borrowed("55P03"), msg);
        }
    }
    // 9.0.3 — COPY's refusals, measured one by one on PostgreSQL 18.6.
    // Many arrive from the option grammar, so ahead of the Parse→42601
    // short-circuit below.
    {
        let msg = e.to_string();
        if let Some(code) = copy_sqlstate(&msg) {
            return (alloc::borrow::Cow::Borrowed(code), msg);
        }
    }
    {
        let msg = e.to_string();
        if msg.contains("is not in select list")
            || msg.contains("must appear in select list")
            || msg.contains("must match initial ORDER BY expressions")
        {
            return (alloc::borrow::Cow::Borrowed("42P10"), msg);
        }
        // v7.39 (round 240) — the other two ON CONFLICT refusals: touching
        // the same row twice in one command is 21000 CARDINALITY_VIOLATION,
        // and DO UPDATE without a conflict target is 42601.
        if msg.contains("cannot affect row a second time") {
            return (alloc::borrow::Cow::Borrowed("21000"), msg);
        }
        // v7.39 (round 241) — a qualifier naming no table in scope is PG's
        // 42P01 UNDEFINED_TABLE, same class as a missing relation.
        if msg.contains("missing FROM-clause entry for table") {
            return (alloc::borrow::Cow::Borrowed("42P01"), msg);
        }
        // v7.39 (round 242) — grouping() over a non-key is PG's 42803
        // GROUPING_ERROR. Parser-raised, so ahead of the Parse→42601
        // short-circuit.
        if msg.contains("arguments to GROUPING must be grouping expressions") {
            return (alloc::borrow::Cow::Borrowed("42803"), msg);
        }
        // v7.39 (round 620) — an ungrouped column is the same class. It used
        // to reach the wire as 42703 UNDEFINED_COLUMN, because the engine
        // reported it as a column that does not exist; now it says what it is,
        // and the code says so too.
        if msg.contains("must appear in the GROUP BY clause") {
            return (alloc::borrow::Cow::Borrowed("42803"), msg);
        }
        // v7.39 (round 620) — a cast target that names no type is PG's 42704
        // UNDEFINED_OBJECT. It used to reach the wire as the generic 42000,
        // under SPG's own wording.
        if msg.contains("does not exist") && strip_error_class(&msg).starts_with("type \"") {
            return (alloc::borrow::Cow::Borrowed("42704"), msg);
        }
        // v7.39 (round 244) — sequence-range errors: a setval outside the
        // range is 22003 NUMERIC_VALUE_OUT_OF_RANGE, the CREATE SEQUENCE
        // option refusals 22023.
        if msg.contains("is out of bounds for sequence") {
            return (alloc::borrow::Cow::Borrowed("22003"), msg);
        }
        if msg.contains("cannot be less than MINVALUE")
            || msg.contains("cannot be greater than MAXVALUE")
            || msg.contains("INCREMENT must not be zero")
        {
            return (alloc::borrow::Cow::Borrowed("22023"), msg);
        }
        if msg.contains("ON CONFLICT DO UPDATE requires inference specification") {
            return (alloc::borrow::Cow::Borrowed("42601"), msg);
        }
        if msg.contains("query must have the same number of columns") {
            return (alloc::borrow::Cow::Borrowed("42601"), msg);
        }
        // v7.39 (round 239) — the row-count clause errors come from the
        // parser, so they must be classified ahead of the Parse
        // short-circuit below: a negative LIMIT is PG's 2201W, a negative
        // OFFSET 2201X, and a literal that won't coerce to bigint 22P02.
        if msg.contains("LIMIT must not be negative") {
            return (alloc::borrow::Cow::Borrowed("2201W"), msg);
        }
        if msg.contains("OFFSET must not be negative") {
            return (alloc::borrow::Cow::Borrowed("2201X"), msg);
        }
        if msg.contains("invalid input syntax for type bigint") {
            return (alloc::borrow::Cow::Borrowed("22P02"), msg);
        }
        // v7.39 (round 233) — two set-operation branch columns with no
        // common type are PG's 42804 DATATYPE_MISMATCH.
        if msg.contains(" types ") && msg.contains(" cannot be matched") {
            return (alloc::borrow::Cow::Borrowed("42804"), msg);
        }
    }
    // v7.39 (read01 round 230) — window-clause errors carry PG's own class
    // and must be classified BEFORE the two variant-level short-circuits
    // below: the named-window complaints are raised by the parser (which
    // would otherwise blanket them as 42601) and the frame ones arrive as
    // `Unsupported` (whose Display prefixes "unsupported: ", so the message
    // arms further down only ever see a substring).
    if let Some(code) = window_sqlstate(&e.to_string()) {
        return (alloc::borrow::Cow::Borrowed(code), e.to_string());
    }
    // v7.39 (read01 round 95) — a parse failure is PG's 42601 SYNTAX_ERROR
    // (was the generic 42000). The character position rides the separate `P`
    // field (see parse_error_position).
    if let EngineError::Parse(_) = e {
        return (alloc::borrow::Cow::Borrowed("42601"), e.to_string());
    }
    let msg = e.to_string();
    // Map constraint violations to their PG SQLSTATE class-23 codes so
    // clients can branch on them (23505 for a duplicate key, 23502 for a
    // NOT NULL, 23503 for a foreign key, 23514 for a CHECK) instead of the
    // generic 42000. Match the engine's violation phrasings; the
    // "violation" / "NOT NULL column" qualifiers keep DDL errors that merely
    // mention a constraint kind from being misclassified.
    let code =
        // v7.37.17 (Phase E3) — isolation switch after the tx's first
        // query: PG's 25001 ACTIVE_SQL_TRANSACTION.
        if msg.contains("cannot drop the currently open database") {
            // PG 18.4, measured: 55006 OBJECT_IN_USE.
            "55006"
        } else if msg.contains("database \"") && msg.ends_with("does not exist") {
            // `contains`, not `starts_with`: EngineError::Unsupported's
            // Display prefixes "unsupported: ", so every arm in this
            // function only ever sees the wording as a substring.
            // PG 18.4, measured: 3D000 INVALID_CATALOG_NAME — a distinct
            // code from the 42P01 an unknown table gets, and clients that
            // branch on it are asking about the database specifically.
            "3D000"
        } else if msg.contains("in a read-only transaction") {
            // v7.39 — PG's 25006 READ_ONLY_SQL_TRANSACTION, taken from
            // `\set VERBOSITY verbose` on PG 18.6. Covers the statement
            // form (`cannot execute INSERT in a read-only transaction`)
            // and the sequence-function form (`cannot execute nextval()
            // in a read-only transaction`), which PG spells the same way.
            "25006"
        } else if msg.contains("must be called before any query")
            // v7.40.12 — PG spells the read-write half of the same rule
            // with a different verb: "transaction read-write mode must
            // be SET before any query" (measured, 25001). Matching only
            // the "called" wording would have left the new refusal on
            // the generic 42000.
            || msg.contains("must be set before any query")
            // v7.40.12 — the subtransaction siblings, each with PG's own
            // verb: "must not be called in a subtransaction", "cannot be
            // called within a subtransaction", and the read-write one
            // that names neither. All three measured at 25001.
            || msg.contains("subtransaction")
            || msg.contains("cannot set transaction read-write mode")
            // PG's PreventInTransactionBlock family — VACUUM, ALTER
            // SYSTEM, CREATE DATABASE, the CONCURRENTLY index forms,
            // DISCARD ALL. All 25001, all phrased this way.
            || msg.contains("cannot run inside a transaction block")
        {
            "25001"
        // v7.39 (bpchar epic) — CHAR(n)/VARCHAR(n) overflow is PG's 22001
        // STRING_DATA_RIGHT_TRUNCATION.
        } else if msg.contains("value too long for type") {
            "22001"
        // v7.39 (GUC knife 5) — PG's datetime input errors: field values
        // that don't fit the calendar/DateStyle are 22008
        // DATETIME_FIELD_OVERFLOW; malformed text is 22007
        // INVALID_DATETIME_FORMAT.
        } else if msg.contains("date/time field value out of range")
            // v7.39 (read01 timestamp.c) — arithmetic range family.
            || msg.contains("timestamp out of range")
            || msg.contains("interval out of range")
        {
            "22008"
        } else if msg.contains("invalid input syntax for type date")
            || msg.contains("invalid input syntax for type timestamp")
        {
            "22007"
        // v7.39 (read01 utils/adt) — the generic bad-literal class
        // (boolean, money, …) is 22P02 INVALID_TEXT_REPRESENTATION.
        } else if msg.contains("invalid input syntax for type")
            || msg.contains("invalid Roman numeral")
            || msg.contains("invalid cidr value")
        {
            "22P02"
        // v7.39 (read01 utils/adt, float.c) — inverse-trig domain
        // violations (asind(2)) are 22003 NUMERIC_VALUE_OUT_OF_RANGE.
        } else if msg.contains("input is out of range")
            || msg.contains("integer out of range")
            || msg.contains("smallint out of range")
            || msg.contains("bigint out of range")
            || msg.contains("value overflows numeric format")
            || msg.contains("OID out of range")
            || msg.contains("is out of range for type double precision")
            || msg.contains("is out of range for type real")
            // v7.39 (read01 orderedsetaggs.c) — percentile fraction range.
            || msg.contains("is not between 0 and 1")
            // r193 — numeric(p,s) precision overflow (PG's exact text).
            || msg.contains("numeric field overflow")
            // v7.39 (round 467) — MySQL's unsigned arithmetic range check.
            // MariaDB spells it 1690 / 22003; the mysqlwire errno map reads
            // the SQLSTATE this assigns.
            || msg.contains("BIGINT UNSIGNED value is out of range")
        {
            "22003"
        // v7.39 (round 470) — MySQL's "omitted column has no default"
        // (1364 / HY000). MySQL-only: a PG session never raises it, so
        // carrying MySQL's own SQLSTATE here costs a PG client nothing and
        // lets the mysqlwire errno table find it.
        } else if msg.contains("doesn't have a default value") {
            "HY000"
        // v7.39 (read01 int.c) — PG's 22012 DIVISION_BY_ZERO.
        } else if msg.contains("division by zero") {
            "22012"
        // v7.39 (read01 numeric.c) — log/power domain violations carry
        // their SQL-spec-mandated states.
        // v7.39 (read01 rangetypes.c) — range construction rejections are
        // PG's 22000 DATA_EXCEPTION.
        } else if msg.contains("range lower bound must be less than or equal")
            || msg.contains("result of range difference would not be contiguous")
            || msg.contains("result of range union would not be contiguous")
        {
            "22000"
        } else if msg.contains("malformed range literal")
            || msg.contains("is not a valid binary digit")
        {
            "22P02"
        // v7.39 (read01 varbit.c) — bit-string length mismatch family.
        } else if msg.contains("bit strings of different sizes") {
            "22026"
        // v7.39 (read01 varlena.c) — 22011 SUBSTRING_ERROR.
        } else if msg.contains("negative substring length not allowed") {
            "22011"
        // v7.39 (read01 varlena.c) — byte/bit index out of range.
        } else if msg.contains("out of valid range, 0..") {
            "22003"
        // v7.39 (read01 regexp.c) — 2201B INVALID_REGULAR_EXPRESSION.
        } else if msg.contains("invalid regular expression") {
            "2201B"
        // v7.39 (read01 regproc.c) — name-resolution errors.
        } else if msg.contains("more than one function named")
            || msg.contains("more than one operator named")
        {
            "42725"
        } else if (msg.contains("type \"") && msg.contains("\" does not exist"))
            || msg.contains("text search configuration \"")
            || msg.contains("text search dictionary \"")
            // v7.39 (read01 round 89) — a missing index is PG's 42704
            // UNDEFINED_OBJECT (DROP INDEX / pg_get_indexdef on a bad name).
            || (msg.contains("index \"") && msg.contains("\" does not exist"))
            // r1038 — so is an operator class CREATE INDEX names and
            // pg_opclass does not have. Verified against PG18.4:
            // `operator class "weird_garbage" does not exist for access
            // method "gin"`, 42704.
            || msg.contains("operator class \"")
            // v7.40.12 — and a GUC name nothing recognises. Measured on
            // PG 18.6: `SHOW spam_x` -> 42704, `find_option, guc.c:1276`.
            // SPG reported PG's sentence under the generic 42000, so a
            // driver that keys on the class saw a stranger where it had
            // just been given PG's own words. Found while removing the
            // wire's SHOW shortcut, which used to answer this name with
            // an empty row and never reached the error at all.
            || msg.contains("unrecognized configuration parameter \"")
            // 8.0.3 — `DROP EXTENSION nosuch`, measured 42704 on 18.6.
            || (strip_error_class(&msg).starts_with("extension \"")
                && msg.ends_with("\" does not exist"))
        {
            "42704"
        // v7.39 (read01 round 89) — a column named twice in an INSERT target
        // list is PG's 42701 DUPLICATE_COLUMN.
        } else if msg.contains("\" specified more than once") {
            "42701"
        } else if (msg.contains("function \"") && msg.contains("\" does not exist"))
            || msg.contains("operator does not exist:")
            // v7.39 (read01 round 77) — a named argument aimed at a function
            // that declares no such parameter is PG's 42883 too ("function
            // lpad(string => unknown, …) does not exist"): no candidate matches
            // the call. SPG names the reason instead of the missing candidate,
            // but a driver must still read the same class.
            || msg.contains("does not support named arguments")
            || msg.contains("has no argument named")
        {
            "42883"
        // v7.39 (read01 round 45, commands/) — DDL object errors.
        // A second PRIMARY KEY is PG's 42P16 INVALID_TABLE_DEFINITION.
        // v7.38.22 — a COLLATE on a type that cannot carry one is PG's
        // 42804 DATATYPE_MISMATCH, not a syntax or feature class:
        //
        //     ERROR:  42804: collations are not supported by type integer
        //
        // Measured on PostgreSQL 18.4 for `ORDER BY … COLLATE` and for the
        // column declaration; both raise the same code, so both map here.
        } else if msg.contains("collations are not supported by type") {
            "42804"
        // 9.0.0 — an index that cannot back a PRIMARY KEY / UNIQUE
        // constraint is PostgreSQL's 42809 WRONG_OBJECT_TYPE, measured on
        // 18.6 for all three refusals of `ADD CONSTRAINT … USING INDEX`.
        // They reached the client as 42000, the class code.
        } else if msg.contains("Cannot create a primary key or unique constraint using such an index") {
            "42809"
        } else if msg.contains("multiple primary keys for table")
            // v7.38.19 — a column declared with a pseudo-type is the same
            // class: the table definition is invalid, not the type
            // undefined.
            || msg.contains("has pseudo-type")
        {
            "42P16"
        // A GENERATED ALWAYS identity/column explicit-value insert is PG's
        // 428C9 GENERATED_ALWAYS.
        } else if msg.contains("cannot insert a non-DEFAULT value into column") {
            "428C9"
        // A duplicate column on ALTER TABLE ADD COLUMN is 42701
        // DUPLICATE_COLUMN; a missing column is 42703 UNDEFINED_COLUMN.
        } else if msg.contains("column \"") && msg.contains("already exists") {
            "42701"
        } else if msg.contains("column \"") && msg.contains("does not exist") {
            "42703"
        // v7.39.2 — PG's QUALIFIED form carries no quotes: `column
        // ea.no_such does not exist`. The quoted pattern above cannot
        // see it, and without this the state fell through to the
        // generic one.
        // 9.0.0 — on the STRIPPED message. The classification runs on the
        // full Display, which starts with the error's class (`eval: `), so
        // a rule anchored at the start never fired: a qualified column PG
        // reports as 42703 reached the wire as the generic 42000.
        } else if strip_error_class(&msg).starts_with("column ")
            && msg.ends_with(" does not exist")
        {
            "42703"
        // v7.39 (read01 round 47) — constraint errors must be classified
        // BEFORE the table/relation patterns below: PG's RENAME CONSTRAINT
        // wording ("constraint \"c\" for table \"t\" does not exist") also
        // contains `table "`, which would otherwise steal it for 42P01.
        // A duplicate object (constraint / type) is 42710; a missing one is
        // 42704 UNDEFINED_OBJECT.
        // The dup-constraint pattern must be narrow: PG's 23505 duplicate-key
        // message also carries `constraint "t_pkey"` and a DETAIL ending in
        // "already exists.", so key on PG's distinctive "for relation" /
        // "for table" qualifier, which only the DDL form has.
        } else if msg.contains("constraint \"")
            && (msg.contains("\" for relation \"") || msg.contains("\" for table \""))
            && msg.contains("already exists")
        {
            "42710"
        } else if msg.contains("constraint \"") && msg.contains("does not exist") {
            "42704"
        } else if msg.contains("type \"") && msg.contains("already exists") {
            "42710"
        // 8.0.3 — `CREATE EXTENSION` of an installed one, 42710 on 18.6.
        } else if strip_error_class(&msg).starts_with("extension \"")
            && msg.ends_with("\" already exists")
        {
            "42710"
        // v7.39 (read01 round 49) — ALTER TYPE ADD VALUE / RENAME VALUE.
        } else if msg.contains("enum label \"") && msg.contains("already exists") {
            "42710"
        // v7.39 (read01 round 235) — jsonpath strict-mode refusals each
        // carry their own SQLSTATE in PG's SQL/JSON classes, not one
        // shared code: a missing key / non-object accessor is 2203A
        // SQL_JSON_MEMBER_NOT_FOUND, an out-of-range subscript 22033
        // INVALID_SQL_JSON_SUBSCRIPT, a wildcard on a non-array 22039
        // SQL_JSON_ARRAY_NOT_FOUND.
        } else if msg.contains("JSON object does not contain key")
            || msg.contains("jsonpath member accessor can only be applied")
        {
            "2203A"
        } else if msg.contains("jsonpath array subscript is out of bounds") {
            "22033"
        } else if msg.contains("jsonpath wildcard array accessor can only be applied")
            || msg.contains("jsonpath array accessor can only be applied")
        {
            "22039"
        } else if msg.contains("is not an existing enum label")
            // v7.39 (read01 round 234) — the jsonb modification family's
            // refusals are PG's 22023 INVALID_PARAMETER_VALUE too.
            || msg.contains("cannot delete from scalar")
            || msg.contains("cannot delete path in scalar")
            || msg.contains("cannot set path in scalar")
            || msg.contains("cannot delete from object using integer index")
        {
            "22023"
        // DROP IDENTITY on a plain column.
        } else if msg.contains("is not an identity column") {
            "42703"
        // A duplicate relation (table / index / view / sequence) is 42P07.
        } else if msg.contains("relation \"") && msg.contains("already exists") {
            "42P07"
        // DROP TABLE on a missing table is 42P01 UNDEFINED_TABLE; every
        // other path (SELECT / ALTER / …) says "relation", same state.
        } else if (msg.contains("table \"")
            || msg.contains("relation \"")
            // v7.39 (read01 round 89) — a missing view is PG's 42P01 too.
            || msg.contains("view \"")
            // v7.39 (round 698) — and a missing SEQUENCE, which PG also
            // answers 42P01 for (`sequence "s" does not exist`).
            //
            // Leaving it out cost more than the code: an unclassified error
            // stays 42000, and the 42000 branch below is the one that does
            // NOT strip SPG's internal prefixes. The sequence not-found
            // rides `StorageError::Corrupt`, whose Display prefixes
            // `corrupt on-disk format: ` — so `DROP SEQUENCE nosuch`
            // reported a CORRUPTION to the client. An operator reading that
            // goes looking for a damaged file.
            || msg.contains("sequence \""))
            && msg.contains("does not exist")
        {
            "42P01"
        } else if msg.contains("cannot take logarithm of") {
            "2201E"
        } else if msg.contains("zero raised to a negative power is undefined")
            || msg.contains("a negative number raised to a non-integer power")
            || msg.contains("cannot take square root of a negative number")
        {
            "2201F"
        // v7.39 (read01 json.c) — 22030 DUPLICATE_JSON_OBJECT_KEY_VALUE.
        } else if msg.contains("duplicate JSON object key value") {
            "22030"
        // v7.39 (read01 like_match.c) — 22025 INVALID_ESCAPE_SEQUENCE.
        } else if msg.contains("LIKE pattern must not end with escape")
            || msg.contains("invalid escape string")
        {
            "22025"
        // v7.39 (read01 oracle_compat.c) — chr() limits are PG's 54000
        // PROGRAM_LIMIT_EXCEEDED.
        } else if msg.contains("null character not permitted")
            || msg.contains("requested character too large for encoding")
        {
            "54000"
        // v7.39 (tz epic) — bad GUC values (TimeZone / DateStyle /
        // IntervalStyle / extra_float_digits range) are PG's 22023
        // INVALID_PARAMETER_VALUE.
        } else if msg.contains("invalid value for parameter")
            || msg.contains("is outside the valid range for parameter")
            || msg.contains("sample size must be between")
            // v7.39 (ts_headline validation) — PG's headline option
            // errors are 22023 INVALID_PARAMETER_VALUE.
            || msg.contains("unrecognized headline parameter")
            || msg.contains("MinWords must be")
            || msg.contains("ShortWord must be")
            || msg.contains("MaxFragments must be")
            || msg.contains("step size cannot equal zero")
            || msg.contains("field position must not be zero")
            // v7.39 (read01 numeric.c) — generate_series(numeric) bound /
            // step rejections.
            || msg.contains("start value cannot be")
            || msg.contains("stop value cannot be")
            || msg.contains("step size cannot be")
            || msg.contains("cannot get array length of")
            || msg.contains("cannot call json_object_keys")
            || msg.contains("cannot call jsonb_object_keys")
            || msg.contains("string is not a valid identifier")
            // v7.39 (read01 round 51) — has_table_privilege's privilege word.
            || msg.contains("unrecognized privilege type")
            // v7.39 (round 253) — an unknown EXTRACT/date_part field name.
            || (msg.contains("unit \"") && msg.contains("\" not recognized for type"))
        {
            "22023"
        // v7.39 (ts_headline validation) — a malformed key=value list is
        // PG's 42601 SYNTAX_ERROR.
        } else if msg.contains("invalid parameter list format")
            || msg.contains("of jsonpath input")
            // v7.39 (read01 round 88) — INSERT value/column arity mismatch is
            // PG's 42601 SYNTAX_ERROR.
            || msg.contains("INSERT has more expressions than target columns")
            || msg.contains("INSERT has more target columns than expressions")
        {
            "42601"
        // v7.39 (read01 utils/adt) — PG's multidim-search refusal is
        // 0A000 FEATURE_NOT_SUPPORTED.
        } else if msg.contains("searching for elements in multidimensional arrays")
            || msg.contains("encoding conversion from UTF8 to ASCII")
            // v7.39 (read01 pseudotypes.c) — dummy pseudotype input funcs.
            || msg.contains("cannot accept a value of type")
            // v7.39 (round 150) — referencing a no-RETURNING data-modifying
            // CTE (parse_relation.c addRangeTableEntryForCTE).
            || msg.contains("does not have a RETURNING clause")
            // v7.39 (round 151) — the modifying-CTE placement rules: nested
            // WITH (parse_cte.c) and view / matview bodies (view.c,
            // analyze.c). Round 81 introduced the first message with the
            // then-default 42000; PG uses 0A000 for all three.
            || msg.contains("must be at the top level")
            || msg.contains("must not contain data-modifying statements in WITH")
            || msg.contains("must not use data-modifying statements in WITH")
            // v7.39 (round 154) — writes targeting a computed view column.
            || msg.contains("View columns that are not columns of their base relation")
            // v7.39 (round 247) — the COPY option refusals share the class.
            || msg.contains("requires CSV mode")
            || msg.contains("must be a single one-byte character")
            // v7.39 (round 253) — EXTRACT field/type validity (PG 0A000).
            || (msg.contains("unit \"") && msg.contains("\" not supported for type"))
        {
            "0A000"
        } else if msg.contains("duplicate key value violates unique constraint")
            || (msg.contains("violation") && (msg.contains("UNIQUE") || msg.contains("PRIMARY KEY")))
            // v7.39 (read01 round 52) — CREATE UNIQUE INDEX over duplicate rows.
            || msg.contains("could not create unique index")
        {
            "23505"
        // v7.39 (round 210) — EXCLUDE constraint violation (PG 23P01
        // exclusion_violation).
        } else if msg.contains("violates exclusion constraint") {
            "23P01"
        // v7.39 (round 220) — a CYCLE-less sequence past its bound
        // (PG 2200H sequence_generator_limit_exceeded).
        } else if msg.contains("nextval: reached") {
            "2200H"
        } else if msg.contains("violates foreign key constraint")
            || msg.contains("FOREIGN KEY violation")
        {
            "23503"
        } else if msg.contains("violates check option") {
            // v7.39 (round 132) — WITH CHECK OPTION violation.
            "44000"
        } else if msg.contains("violates check constraint")
            || msg.contains("CHECK constraint violation")
            // 9.0.4 — the ALTER-time wording. PostgreSQL 18.6 answers
            // `ALTER TABLE … ADD CONSTRAINT … CHECK` over rows that do
            // not satisfy it with the same 23514 it uses at INSERT
            // time; SPG produced the sentence byte for byte and left
            // the code on the catch-all, so a client catching 23514 to
            // say "your data does not satisfy this" caught nothing.
            || msg.contains("is violated by some row")
        {
            "23514"
        } else if msg.contains("violates not-null constraint")
            || msg.contains("NOT NULL column")
            // v7.39 (read01 round 49) — SET NOT NULL over existing NULLs.
            || msg.contains("contains null values")
        {
            "23502"
        // v7.39 (SQLSTATE fidelity) — file-access failures map like
        // PG's errcode_for_file_access(): ENOSPC/EDQUOT -> 53100
        // disk_full, ENOMEM -> 53200 out_of_memory, EACCES/EPERM ->
        // 42501 insufficient_privilege, ENOENT -> 58P01
        // undefined_file, anything else on the durability path ->
        // 58030 io_error. Matched on the OS message the io::Error
        // Display carries.
        } else if msg.contains("durability append failed") || msg.contains("could not write") {
            let lower = msg.to_ascii_lowercase();
            if lower.contains("no space left")
                || lower.contains("quota")
                || lower.contains("storage full")
                || lower.contains("below water-mark")
            {
                "53100"
            } else if lower.contains("out of memory") {
                "53200"
            } else if lower.contains("permission denied") {
                "42501"
            } else if lower.contains("no such file") {
                "58P01"
            } else {
                "58030"
            }
        // v7.39 (read01 round 57) — the table-privilege failures PG raises as
        // 42501 insufficient_privilege, and the unknown-role one (42704).
        } else if msg.contains("permission denied for table")
            || msg.contains("permission denied for sequence")
            || msg.contains("permission denied for schema")
            || msg.contains("permission denied for function")
            || msg.contains("must be owner of table")
        {
            "42501"
        } else if msg.contains("role \"") && msg.contains("does not exist") {
            "42704"
        // v7.39 (read01 round 58) — DROP ROLE with grants still pointing at it
        // (PG 2BP01 dependent_objects_still_exist).
        // 9.0.4 — PostgreSQL files both of these under 0A000: emptying
        // a table another one references, and changing the type of a
        // column a view reads. Neither has a way to say "do it anyway",
        // which is what 0A000 means here.
        } else if msg.contains("cannot truncate a table referenced in a foreign key constraint")
            || msg.contains("cannot alter type of a column used by a view or rule")
        {
            "0A000"
        } else if msg.contains("cannot be dropped because some objects depend on it")
            // 9.0.4 — and the wording every DROP of a relation, column
            // or schema uses. SPG produced PostgreSQL's sentence, its
            // DETAIL and its HINT byte for byte and left the code on the
            // catch-all 42000 — so a migration tool that branches on
            // 2BP01 to run the CASCADE form saw an unclassified error.
            || msg.contains("because other objects depend on it")
        {
            "2BP01"
        // v7.39 (read01 round 62) — an overloaded name with no signature to
        // disambiguate it (PG 42725 ambiguous_function).
        } else if msg.contains("is not unique") {
            "42725"
        // …and a call / GRANT / DROP naming a function that has no such
        // signature (PG 42883 undefined_function).
        } else if msg.contains("function") && msg.contains("does not exist") {
            "42883"
        // v7.39 (round 622, S05a) — a value of the wrong type reaching a
        // function, an operator or a cast was classified 42000, which is the
        // CLASS code and carries no information: a client dispatching on
        // SQLSTATE — which is the entire point of SQLSTATE — could not tell
        // "no such function" from a syntax error. Only the handful of
        // messages that happen to spell PG's own sentence (the arm above)
        // ever reached 42883.
        //
        // Measured against PG18 over 37 wrong-type shapes — calls,
        // aggregates, operators, subscripts, json accessors and casts — PG
        // answers exactly three codes: 42846 CANNOT_COERCE for a cast with
        // no path, 22023 for a jsonb value that cannot become the target,
        // and 42883 UNDEFINED_FUNCTION for everything else, since to PG
        // every one of them is "no candidate matches this call". The
        // input-syntax family (22P02 / 22007) is already answered by its
        // own rules above and does not reach here.
        // v7.37 (round 823) — a bare column name that matches more than one
        // relation in a join. SPG raised it through TypeMismatch, so it landed
        // on 42883 UNDEFINED_FUNCTION below — a client dispatching on SQLSTATE
        // was told "no such function" for what is a name-resolution problem.
        // PG18, measured over `SELECT v FROM a x JOIN b y ON x.id=y.id` where
        // both relations have `v`, answers 42702 AMBIGUOUS_COLUMN. The message
        // is PG's own sentence now, so this keys on it.
        } else if msg.contains("column reference") && msg.contains("is ambiguous") {
            "42702"
        // 9.0.0 — `argument of WHERE must be type boolean` is PostgreSQL's
        // 42804 DATATYPE_MISMATCH, measured on 18.6. It reached the client
        // as 42883 UNDEFINED_FUNCTION, from the blanket arm below.
        } else if matches!(
            e,
            EngineError::Eval(crate::eval::EvalError::NotBoolean { .. })
        ) {
            "42804"
        } else if matches!(
            e,
            EngineError::Eval(crate::eval::EvalError::TypeMismatch { .. })
        ) {
            if msg.contains("cannot cast jsonb") {
                "22023"
            } else if msg.contains("cannot cast") {
                "42846"
            } else {
                "42883"
            }
        } else {
            "42000"
        };
    // v7.39 (read01 round 79) — PG's ErrorResponse carries the message ALONE.
    // SPG's `Display for EngineError` prefixes the internal error class
    // ("eval: type mismatch: …", "unsupported: …", "parse: …"), which is useful
    // in a Rust backtrace and is noise — and a visible non-PG-ism — on the wire:
    // every error a client saw was prefixed with SPG's own vocabulary. Strip the
    // class here, at the boundary, so the Rust-facing Display keeps it. The
    // SQLSTATE classification above matched on the full string, so it is
    // unaffected.
    let msg = strip_error_class(&msg);
    (alloc::borrow::Cow::Borrowed(code), msg)
}

/// 8.0.3 — an engine message split the way a client receives it: the
/// primary text, then the `DETAIL` and `HINT` the wire sends as their own
/// fields. Shared by the wire and PL/pgSQL's `SQLERRM`, which reads the
/// primary text alone.
pub fn split_detail_and_hint(msg: &str) -> (&str, Option<&str>, Option<&str>) {
    let (msg, hint) = match msg.split_once("\nHINT:  ") {
        Some((m, h)) => (m, Some(h)),
        None => (msg, None),
    };
    match msg.split_once(" DETAIL: ") {
        Some((m, d)) => (m, Some(d), hint),
        None => (msg, None, hint),
    }
}

/// 8.0.3 — PG's 23505 and 23P01 messages carry no table suffix; the table
/// travels in its own field. The engine appends ` on table "t"` so that
/// field can be lifted, and this is where a client-facing message drops it.
pub fn without_table_suffix<'a>(sqlstate: &str, msg: &'a str) -> &'a str {
    match (sqlstate, msg.find(" on table \"")) {
        ("23505" | "23P01", Some(cut)) => &msg[..cut],
        _ => msg,
    }
}
