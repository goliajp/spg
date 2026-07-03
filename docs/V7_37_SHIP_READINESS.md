# v7.37 ship readiness — what's in, what's queued

> Authoritative snapshot of v7.37's ship surface vs the items deferred
> to v7.38+. The complete-roadmap file
> (`.claude/notes/v7.37.x-complete-roadmap.md`, gitignored) is an
> EXHAUSTIVE PG-completeness audit covering v7.37 through v7.51+;
> this doc is the SHIPPING subset. Re-read after every release.

## v7.37 — what ships

### Catalog completeness

37 PG-shape catalog views land in v7.37, each with the
PG-canonical column set so dashboards / ORMs / pg_dump round-trip
without errors:

`pg_class`, `pg_attribute`, `pg_index`, `pg_constraint`, `pg_proc`,
`pg_type`, `pg_enum`, `pg_namespace`, `pg_database`, `pg_roles`,
`pg_am`, `pg_collation`, `pg_inherits`, `pg_depend`, `pg_publication`,
`pg_subscription`, `pg_replication_slots`, `pg_statistic_ext`,
`pg_statistic`, `pg_stat_statements`, `pg_stat_database`,
`pg_stat_user_tables`, `pg_stat_user_indexes`, `pg_stat_user_functions`,
`pg_stat_io`, `pg_stat_bgwriter`, `pg_stat_archiver`,
`pg_stat_replication`, `pg_stat_progress_vacuum`,
`pg_stat_progress_create_index`, `pg_stat_progress_analyze`,
`pg_tablespace`, `pg_statio_user_tables`, +
`information_schema.{schemata, views, table_constraints, domains,
attributes}`.

### Type completeness

PG 18 builtin scalar coverage: 100% (v7.37.5 milestone). UUID +
INTERVAL + 13 array-of-scalar + 6 multirange + 7 geometry + INET /
CIDR / MACADDR / MACADDR8 / BIT / VARBIT / XML / "char" / MONEY[] all
shipped. Composite + domain meta-types queue with ζ-B (v7.37 SHIP
not gated on those).

### SQL surface

- ALTER TABLE 70+ sub-commands accept-or-execute (18.1-18.18 all
  closed; PG-only forms accept-and-no-op for pg_dump round-trip).
- EXPLAIN (ANALYZE / BUFFERS / TIMING / SETTINGS / WAL / SUGGEST /
  VERBOSE / FORMAT text|json|xml|yaml) all live.
- Partition DDL: LIST + RANGE + HASH strategies; ATTACH / DETACH /
  DETACH CONCURRENTLY all live; pruning on `=` predicates.
- CREATE STATISTICS parse-accepted (v7.17.0 Phase 8 + 23.7
  rationale).
- pg_dump-compat ALTER TABLE residuals (18.18): RESET / OF /
  NOT OF / FORCE RLS / ENABLE/DISABLE ROW LEVEL SECURITY all
  accept-and-no-op.

### Real DML / SQL statements shipped (v7.37.17 autorun slice)

- `TRUNCATE [TABLE] [ONLY] <name>[, ...]
                     [RESTART IDENTITY | CONTINUE IDENTITY]
                     [CASCADE | RESTRICT]` — clears rows via
  Table::truncate(); RESTART IDENTITY parsed but SequenceDef restart
  queues with v7.38.
- `SHOW ALL` returns 13-row `(name, setting, description)` inventory.
- `SHOW <param>` PG-default fallback for 13 GUCs drivers commonly
  probe (lock_timeout / idle_in_transaction_session_timeout /
  transaction_timeout / statement_timeout / client_min_messages /
  default_tablespace / default_table_access_method / row_security /
  check_function_bodies / xmloption / work_mem /
  maintenance_work_mem / max_connections / shared_buffers /
  effective_cache_size / etc.) — pg_settings row shape widened to
  match.

### Real scalar functions shipped (v7.37.17 autorun slice — expanded)

Real implementations, not stubs. Total shipped this cycle:
~560 scalar helpers across 220+ commits, verified against
known vectors or reference values where possible (MySQL and
PG doc vectors, openssl cross-checks, RFC test vectors).

Advanced-SQL scout campaign (tasks #386-#388) — two scout rounds
(window/CTE shapes, string/aggregate corners) drive three closures,
one of them a pre-existing engine bug the pins caught (2980 e2e
green):

- **Window frame EXCLUDE (#386)**: the frame gains an
  EXCLUDE {CURRENT ROW | GROUP | TIES | NO OTHERS} clause
  (FrameExclusion on WindowFrame). NO OTHERS is the default no-op;
  CURRENT ROW drops the current row from the aggregate frame so
  sum/count/avg over the whole partition EXCLUDE CURRENT ROW see
  everyone else. GROUP / TIES need peer-group awareness the
  aggregate loop lacks, so they error honestly.
- **VALUES-seed recursive CTE (#387)**: a VALUES seed can head the
  set-operation chain inside a CTE body, so WITH RECURSIVE t(n) AS
  (VALUES(1) UNION ALL SELECT n+1 FROM t WHERE n<5) parses like the
  SELECT-seed form — the VALUES arm attaches trailing UNION /
  INTERSECT / EXCEPT peers via the shared setop-chain parser.
- **jsonb_object_agg dedup (#388)**: jsonb is a map, so
  jsonb_object_agg over duplicate keys keeps the last value — it
  used to emit an object with repeated keys (invalid jsonb). The
  finalize now dedups keys in first-seen order, last value winning
  for the jsonb variant; json_object_agg keeps every pair. The
  scout's other fifteen probed shapes (lpad/rpad/left/right/
  split_part/translate/repeat/btrim, string_agg, array_agg
  DISTINCT ORDER BY, count DISTINCT, GROUPING SETS, bit_and/or,
  ntile/row_number, HAVING) were all already supported.

Operator/type scout campaign (tasks #381-#384) — three scout
rounds (operator symbols, datetime units, type IO) drive four
closures, two of them pre-existing engine bugs the pins caught
(2971 e2e green):

- **PG operator symbols (#381)**: the regex match family ~ / ~* /
  !~ / !~* (lexed as new tokens, lowered onto regexp_like with an
  'i' flag for the case-insensitive variants and NOT wrapping for
  the negated ones), ^@ starts-with, ^ power (binding tighter than
  *), # integer XOR via (a|b)-(a&b), and integer << / >> shifts
  claimed ahead of the inet containment interpretation. The pins
  caught regexp_like ignoring its flags argument — ~* / !~* and
  regexp_like(x, p, 'i') were all silently case-sensitive; now the
  compiled tree folds case for the 'i' flag.
- **date_trunc units (#382)**: week (truncates to the ISO Monday),
  quarter, decade, and century/millennium (from year 1, no year 0)
  plus milliseconds/microseconds join the original six units.
- **Type conversions (#383)**: ::text[] accepts any scalar array
  (int/bigint/smallint/bool/float), rendering each element as text;
  TIME ± INTERVAL applies the sub-day microseconds and wraps modulo
  24h (23:30 + 1h = 00:30).
- **Unconstrained numeric cast (#384)**: bare ::numeric used to
  rescale to 0 decimals, dropping the fraction (3.14::numeric → 3).
  Precision 0 is the unambiguous unconstrained sentinel
  (numeric(0,0) is invalid in PG), so both the float and text
  coerce arms now keep the source scale when unconstrained;
  constrained numeric(p,s) still rounds as before.

Expression-idiom scout campaign (tasks #375-#379) — three scout
rounds (function syntax, date/time fields, operators) drive five
closures, with two real engine bugs found and fixed by the
campaign's own pins (2960 e2e green):

- **SQL-standard function syntax (#375)**: TRIM([BOTH|LEADING|
  TRAILING] [chars] FROM str) lowers onto btrim/ltrim/rtrim
  (keyword forms upfront, keyword-less chars form via a first-arg
  FROM hook, comma forms untouched); POSITION(sub IN str) lowers
  onto strpos via a suppress_in_tail parser flag so IN reads as
  the argument separator — regular IN-lists and the position(sub,
  str) comma form (incl. bytea) unaffected. A NUMERIC operand
  paired with an INTERVAL now reaches the interval scaling path
  instead of erroring in fixed-point dispatch.
- **ROW constructors (#376)**: bare ROW(a, b, …) renders as PG
  record text (NULL fields empty, special characters
  double-quoted); followed by a comparison or [NOT] IN the ROW
  keyword joins the paren row-comparison machinery, whose RHS
  sites absorb the keyword too — ROW(a,b) = ROW(c,d) works end to
  end. Single-element ROW(x) is valid, matching PG.
- **EXTRACT field completion (#377)**: DOW/ISODOW/DOY/WEEK/
  ISOYEAR/QUARTER/DECADE/CENTURY/MILLENNIUM/JULIAN/MILLISECOND/
  TIMEZONE[_HOUR|_MINUTE] join the original eight fields in both
  EXTRACT and date_part. WEEK/ISOYEAR run the real ISO 8601
  algorithm (52/53-week years, boundary days roll into the
  neighbouring ISO year); CENTURY/MILLENNIUM count from year 1;
  JULIAN anchors at JD 2440588; TIMEZONE is honestly 0 (UTC
  sessions). Intervals gain the era/subsecond fields and error
  honestly on day-of-week/week/julian. Verified against PG doc
  vectors (2023-01-01 → week 52 of ISO year 2022, JD(2000-01-01)
  = 2451545).
- **OVERLAPS (#378)**: (S1, E1) OVERLAPS (S2, E2) joins the
  row-constructor tail — endpoints normalise with least/greatest
  and the predicate lowers to start1 < end2 AND start2 < end1
  (touching endpoints do not overlap, PG's half-open periods).
  The pins exposed a pre-existing bug: value_cmp_for_min_max fell
  through to Equal for non-integer/float/text/bytes types, so
  greatest()/least() over dates, timestamps, times, bools and
  intervals silently returned their first argument — the ordered
  types now compare for real.
- **Array operators (#379)**: && overlap and @> / <@ containment
  claim array operands ahead of the inet/JSON interpretations
  (guarded arms; JSON containment on JSON operands untouched).
  Integer arrays compare across widths, NULL elements never match,
  and the empty array is contained in everything.

DML-side + statement scout campaign (tasks #364-#373) — three
scout rounds (DML shapes, types/casts, MySQL statements) drive ten
closures riding existing machinery, with three real engine bugs
found and fixed by the campaign's own pins (2947 e2e green):

- **MySQL upserts (#364)**: INSERT … ON DUPLICATE KEY UPDATE
  lowers onto ON CONFLICT machinery (empty conflict target
  resolves to the first unique index; VALUES(col) rewrites to
  EXCLUDED.col); REPLACE INTO rides the same channel with empty
  assignments meaning whole-row replace.
- **Multi-table DML (#365)**: UPDATE … FROM and DELETE … USING
  lower the FROM/USING list into EXISTS(SELECT 1 …) WHERE
  conjuncts; SET expressions referencing list tables become
  correlated scalar subqueries. The pins exposed a pre-existing
  gap — DELETE with any subquery in WHERE hit the &mut borrow
  walk and errored — fixed with an immutable pre-pass computing
  hit positions probed by binary search in the hot walk.
- **UPDATE SET forms (#366)**: SET (a, b) = (row/subquery)
  destructures at parse time; SET col = DEFAULT lowers to a
  __column_default marker resolved against the column's catalog
  default at execution.
- **Writable-CTE outer SELECT (#367)**: WITH d AS (DML …
  RETURNING) SELECT … FROM d routes the outer SELECT through the
  transactional CTE-temps machinery (previously only outer DML).
- **COPY TO STDOUT (#368)**: COPY table [(cols)] TO STDOUT
  streams text-format rows via encode_copy_text_cells — the
  exact inverse of the COPY FROM decoder.
- **Cast targets (#369)**: ::time / ::timetz / ::float4 /
  ::float8 / ::real / ::oid resolve; explicit varchar(n)/char(n)
  casts truncate per PG (bare column definitions still reject
  overlong strings honestly).
- **Literals + statement idioms (#370, #371)**: B'1010' / X'1F'
  bit-string literals pre-lex onto ::bit casts (hex digits expand
  to 4 bits each); GROUP BY 1 positional substitutes the
  projection expression; DESCRIBE/DESC route to SHOW COLUMNS;
  MySQL LIMIT offset, count maps both clauses — the pin caught
  offset-clobbering when only a count was present (fixed with
  take()).
- **Quantified subqueries (#372)**: x op ANY/SOME/ALL (SELECT …)
  — = ANY lowers to InSubquery; other operators parse to AnyAll
  over a ScalarSubquery whose single-column result both resolvers
  materialise into an ARRAY literal for the existing three-valued
  AnyAll eval (uncorrelated upfront, correlated per-row after
  outer substitution; empty → ANY false / ALL true). The pins
  exposed the constant-SELECT path (no FROM) silently ignoring
  WHERE — SELECT 1 WHERE false returned a row — now the one
  conceptual row survives only on a true condition.
- **Interval scaling (#373)**: interval * numeric (either order)
  and interval / numeric per PG interval_mul — months/days
  truncate toward zero, fractional month remainder spills into
  days at 30 days/month, fractional day remainder into
  microseconds ('1 day' * 2.5 = 2 days 12:00:00); division by
  zero errors honestly (previously any interval division
  mis-reported DivisionByZero); unary minus negates all three
  fields checked.

SQL-surface scout campaign (tasks #352-#362) — two scout rounds
(temporary probe files sweeping 10-12 shapes each, deleted after
harvest) drive eleven closures; every arc is a parse-time lowering
or a small executor arm, verified against PG doc vectors where
they exist (2919 e2e green):

- **Parallel-zip SRFs (#352, #354)**: multi-arg unnest(a, b, …)
  zips arrays NULL-padded to the longest through an internal
  __unnest_zip marker + shared builder consumed by both the
  primary executor and the join materialiser; ROWS FROM
  (srf(), srf()) is the SQL-standard spelling over the same
  channel (entries lower to their scalar array forms;
  generate_series has none and errors honestly).
- **TABLESAMPLE (#353)**: BERNOULLI(p) lowers to a per-row
  random() < p/100 WHERE conjunct (exact row-level semantics;
  SYSTEM shares it — no page structure to sample); REPEATABLE
  errors honestly. Scoped through a pending-predicate channel
  parse_bare_select save/restores around nested selects.
- **Statement/clause idioms (#355-#357)**: TABLE name shorthand
  (statement head + set-op peer positions via a shared builder);
  COLLATE absorbs the byte-order spellings and errors on locale
  collations (SPG's single ordering IS the C collation);
  AT TIME ZONE lowers to timezone(zone, ts) — UTC/GMT and ±HH:MM
  offsets shift for real, named zones error (no tzdata);
  ORDER BY USING maps the btree operators onto ASC/DESC;
  OPERATOR([schema.]op) reduces to the plain operator before
  binary dispatch.
- **Expression predicates (#358, #360)**: IS [NOT]
  TRUE/FALSE/UNKNOWN via never-NULL CASE lowerings; LIKE ESCAPE
  rewrites literal patterns to the backslash matcher; BETWEEN
  SYMMETRIC ORs both bound orientations; SIMILAR TO composes
  regexp_like over the anchored similar_to_escape regex (PG doc
  vectors pin full-string anchoring).
- **Named windows (#359)**: WINDOW w AS (…) definitions inline
  into OVER w markers at parse time; the bare-ident alias rule
  gains window/tablesample stopwords (both had been swallowed as
  table aliases).
- **Array slices (#361)**: arr[lo:hi] with clamped 1-based
  inclusive bounds and open ends — a new Expr variant threaded
  through all fourteen exhaustive Expr walkers.
- **Row constructors (#362)**: (a, b) compared with = <> < <= >
  >= expands lexicographically at parse time; (a, b) [NOT] IN
  ((…), …) ORs row equalities. Arity mismatches error.

Analytics + INSERT + SRF-correlation campaign (tasks #338-#350) —
grouping analytics, the INSERT clause tail, and the SRF ordinality
/ correlation story close out, with two real engine gaps found and
fixed by the campaign's own verification pins (2889 e2e green):

- **Analytics (#339-#342)**: SELECT DISTINCT ON (exprs) —
  keep-first dedup at the executor exit (Django's
  .distinct(field) shape); GROUP BY ROLLUP / CUBE / GROUPING SETS
  expand at parse time into UNION ALL peers with dropped keys
  nullified in items + group_by (primary set's keys nullified in
  the HEAD too); GROUPING(keys…) rewrites to the per-set bitmask
  constant, recursively through CASE/operators so subtotal
  labelling works. Three union ORDER BY bugs fixed along the way
  (alias under unions substituted with the HEAD constant;
  positional keys unresolved under Wildcard projections;
  unaliased positional under unions).
- **SQL:2016 predicates (#343)**: IS [NOT] JSON
  [VALUE|OBJECT|ARRAY|SCALAR] lowers to a pg_is_json helper.
- **INSERT clause tail (#344-#345)**: ON CONFLICT ON CONSTRAINT
  <name> resolves pg_dump's conflict-target form against the
  synthetic {t}_pkey / {t}_uniq{i} names; INSERT … DEFAULT VALUES
  lowers to the empty-column-list default-fill path (serials
  advance); OVERRIDING {SYSTEM|USER} VALUE accepted and absorbed
  (SPG always uses statement-supplied values).
- **WITH ORDINALITY (#346-#348)**: unnest and every parser-
  rewritten SRF gain the trailing 1-based BIGINT counter (second
  column-alias entry renames it); generate_series gets the same
  plus AS t(n) column aliasing it previously discarded; a shared
  generate_series_rows builder gives the join-position
  materialiser a generate_series arm (FROM t, generate_series(…)
  was TableNotFound before).
- **Correlated SRFs (#349-#350)**: LATERAL before
  generate_series/unnest parses (keyword absorbed); SRF arguments
  containing qualified column references wrap onto the
  lateral_subquery channel so the per-outer-row machinery drives
  them, with or without the keyword (PG's implicit-correlation
  rule). Two engine gaps closed by the pins: the lateral probe
  fallback synthesised col0 for wrapped SRFs (now derives the
  schema from the SRF ref), and array-valued outer columns never
  substituted (value_to_literal_expr has no array form — now
  materialised as ARRAY[…] element-literal constructors).
  generate_series returns zero rows on NULL bounds (PG
  semantics).
- **CTE hints (#338)**: AS [NOT] MATERIALIZED absorbed; WITH
  t(a) AS (VALUES …) bodies parse onto the VALUES lowering.

Structural-SQL campaign (tasks #321-#336) — three multi-cycle
arcs close out the range type, derived tables and the SQL set
operations, with two real correctness bugs found and fixed along
the way (branch at 309 commits ahead of develop, 2855 e2e green):

- **Ranges, full lifecycle (#325-#328)**: constructor functions
  int4range..tstzrange (until then ranges only entered via
  '::int4range' text casts; bounds flags, NULL = unbounded,
  equal-bounds → empty), multirange constructors, range_agg
  (PG 14 — collect into a multirange) and range_intersect_agg
  (calendar-exact intersection fold; unbounded loses to bounded,
  non-inclusive touch collapses to empty). The #256 bound
  predicates now answer directly off real Range values.
- **Derived tables (#329-#332)**: FROM ( SELECT … ) never parsed —
  subqueries only entered FROM through explicit LATERAL. Now:
  plain derived tables (primary + JOIN positions, UNION tails,
  aggregates over them), AS t(a, b) positional column aliases,
  FROM ( VALUES … ) t(cols) with PG's column1..columnN defaults,
  and the top-level bare VALUES statement with ORDER BY/LIMIT
  tails (shared parse_select_tail_into extraction).
- **Set operations (#333-#336)**: INTERSECT [ALL] and EXCEPT
  [ALL] join UNION (PG multiset semantics — min-count
  intersection, occurrence-cancelling subtraction), INTERSECT
  binds tighter than UNION/EXCEPT (boundary-aware regroup),
  parenthesized groups override precedence, and groups carry
  their own ORDER BY/LIMIT via a derived-table wrap.
- **Correctness fixes surfaced by the campaign's own tests**:
  resolve_order_by_position substituted an ORDER BY alias with
  the HEAD's item expression, so a UNION of constant SELECTs was
  a silent no-sort ('SELECT 3 AS x UNION ALL SELECT 1 ORDER BY x
  DESC' returned unsorted); and positional ORDER BY keys reached
  the union sort unresolved under Wildcard projections. Both
  fixed with regression pins.
- **MySQL closures (#321-#324)**: clock spellings (curdate/
  sysdate/utc_* — the time family renders HH:MM:SS text),
  adddate/subdate/date_sub with bare-day shorthand, get_format's
  manual table + convert_tz offset forms (named zones → NULL,
  faithful to unloaded time-zone tables), to_base64/from_base64
  (76-char wrap), MySQL-shape sha/sha2 hex digests, random_bytes,
  load_file → NULL. tsquery_and/or/not complete the FTS operator
  catalog forms (#323).

SRF + MySQL-JSON campaign (tasks #301-#319) — set-returning
functions land through a parser-rewrite pipeline, and the MySQL
JSON surface goes from 2 to 22 functions in five cycles (branch
at 292 commits ahead of develop, 2814 e2e green):

- **SRF build-out (zero new executors)**: FROM-position
  jsonb_array_elements(_text) + json_ variants,
  jsonb_object_keys, generate_subscripts (multi-arg + reverse),
  string_to_table, regexp_split_to_table — all rewritten in the
  parser onto unnest over scalar TextArray surfaces. jsonb_each /
  json_each / json_each_text complete the each family (the plain
  forms keep jsonb rendering in the value column). PG column-
  naming semantics preserved: OUT-parameter SRFs project `value`,
  the rest take the function name, and a bare table alias on a
  single-column SRF renames the column.
- **FTS**: ts_headline (real match highlighting — stemmed-lexeme
  word wrap with StartSel/StopSel options, Not-subtree exclusion)
  and ts_rewrite (synonym-expansion subtree rewriting).
- **MySQL JSON, complete non-schema surface**: batch 1 non-path
  helpers (json_valid/type/length/keys/depth/quote/unquote/
  array/pretty/storage_size); a MySQL JSON path parser ($ .key
  ."quoted" [N]; wildcards error honestly) powering json_extract
  + json_contains_path; the mutation family json_set/insert/
  replace/remove with two-dialect routing ('$'-paths → MySQL,
  '{a,b}' text-array paths → PG jsonb_set); json_array_append /
  json_array_insert / json_contains (containment recursion);
  json_merge_patch (RFC 7396) / json_merge_preserve /
  json_overlaps; json_search (LIKE-matching path finder) +
  json_value.
- **MySQL misc**: inet_aton/ntoa + inet6_aton/ntoa (RFC 5952
  compression) + is_ipv4/is_ipv6, INSERT() char-window replace,
  FORMAT(X, D) thousands separators (dialect-disambiguated from
  PG's printf format on the first arg's type), NAME_CONST.
- **Datetime**: str_to_date (%-specifier parser, NULL on bad
  input) + time_format; timestampadd/timestampdiff (bare unit
  keywords lowered in the parser, calendar-exact month walks,
  complete-units semantics); PG typed literals DATE '…' /
  TIMESTAMP '…' / TIMESTAMPTZ '…' lowered onto the cast paths.
- **Arrays**: array_append/prepend/cat + PG 14 trim_array;
  string_to_array gains the 3-arg null_string form.
- **Honest deferrals recorded in commits**: MySQL named locks
  (needs a synchronized map; no_std engine has no mutex),
  json_value RETURNING clause (parser syntax), JSON path
  wildcards, TIME as a standalone type, large-object (lo_*) API.

Stub-retirement wave (tasks #281-#299) — a systematic pass
converting NULL/constant stubs into real catalog-backed
implementations, plus aggregate + information_schema growth
(branch at 272 commits ahead of develop, 2741 e2e green):

- **Aggregates**: any_value (PG 16), group_concat (MySQL's
  most-used aggregate, ',' default + scalar coercion), xmlagg,
  SQL:2016 json_arrayagg/json_objectagg spellings
  (differential-tested vs the pg names).
- **pg_get_*def trilogy — real deparse**: pg_get_viewdef
  (catalog view bodies), pg_get_indexdef (CREATE INDEX
  reconstruction, differential-equal to pg_indexes.indexdef),
  pg_get_constraintdef (PK/UNIQUE/FK with ON DELETE/UPDATE
  actions; default action omitted like PG). psql \d and
  pgAdmin panels show real DDL now.
- **Resolver upgrades**: format_type ("unknown" stub → 38-oid
  SQL-standard names + varchar(n)/numeric(p,s)/timestamp(n)
  typmod rendering); to_regclass/to_regtype/to_regnamespace
  (always-NULL → real; the Django/Alembic
  `to_regclass('t') IS NOT NULL` existence check works).
- **Size meters go live**: pg_relation_size family +
  pg_database_size read Table::hot_bytes / hot_tier_bytes
  (the freezer's own meters); pg_class.relpages + pg_relpages
  report 8 KiB-page units off the same counter. Capacity
  dashboards stop reading zero.
- **Sequence surface**: lastval() real (new Engine
  last_sequence_used register; PG's exact not-yet-defined
  error), pg_sequence_last_value real (is_called=false →
  NULL).
- **information_schema growth**: columns widened 7→13
  (column_default incl. serial nextval synthesis,
  numeric_precision/scale, udt_name, is_identity); four new
  views — sequences, check_constraints, triggers (PG-style
  event explosion), constraint_column_usage (FK rows list the
  REFERENCED table's columns, the subtle PG semantic).
- **Misc**: updatability probes (pg_relation_is_updatable → 28
  for tables AND views — views are genuinely updatable via the
  auto-updatable redirect), _pg_* typmod-math internals for
  SQLAlchemy/JDBC, age(xid)/mxid_age wraparound unblock,
  jsonb ?/?|/?&/@>/<@ + ->/->> catalog function names,
  24 *_larger/*_smaller MAX/MIN internals, textcat/byteacat.

Late-cycle additions (tasks #251-#279) on top of the sections
below:

- **Regex engine backtracking fix** — quantifier matching was
  greedy-without-backtracking (self-documented v7.17 stop-gap);
  'bar.*que' failed against 'foobarbequebaz'. New re_match_seq
  backtracking sequence matcher corrects the whole regexp
  family. + `regexp_match` (PG 10+ singular form).
- **pgcrypto real crypt()** — FreeBSD md5crypt ('$1$'), vector
  verified against `openssl passwd -1`; gen_salt('md5') real
  random salt; armor/dearmor real RFC 4880 ASCII armor with
  CRC-24; bcrypt/DES/PGP-encryption error honestly.
- **to_date / to_timestamp(text, fmt)** — real format-template
  parser (YYYY/MM/DD/HH24/HH12/MI/SS/MS/US + MON/MONTH + AM/PM);
  to_date previously had NO implementation.
- **MySQL-compat surface (7 batches)** — locate/instr/
  substring_index/find_in_set/elt/field/space; hex/unhex/conv/
  bin/oct/ord + mid/lcase/ucase; dayname/monthname/dayofweek/
  dayofyear/weekofyear/last_day/datediff/strcmp; quarter/
  to_days/from_days/makedate/yearweek; time_to_sec/sec_to_time/
  maketime/addtime/subtime/timediff/microsecond; day/month/
  year/hour/minute/second/weekday/week + period_add/period_diff;
  quote/export_set/make_set/truncate; rand/connection_id/
  uuid_short/is_uuid + session probes. All MySQL doc vectors.
- **Operator↔function-name pairing** — json_object_field /
  json_array_element (+_text, jsonb_ twins, 8 names) for -> ->>;
  jsonb_exists/_any/_all/contains/contained for ? ?| ?& @> <@;
  textcat/byteacat for ||; 24 *_larger/*_smaller MAX/MIN
  internals.
- **FTS completion** — ts_delete/ts_filter/tsquery_phrase;
  tsvector_to_array/array_to_tsvector/get_current_ts_config/
  ts_lexize; json(b)_to_tsvector.
- **Correctness fixes** — range bound predicates (lower_inc
  etc.) upgraded from wrong constant-false stubs to real
  text-form parsing + range_merge; age(xid) overload unblocks
  wraparound monitoring (was a type error).
- **Misc real** — cash_words (number-to-English), to_ascii
  (NFD accent strip), inet_merge (common-prefix), macaddr8_set7bit,
  xmltext/xmlconcat, jsonb_set_lax (all 4 null treatments),
  acldefault/makeaclitem, cot/log2, uuidv7 + uuid v3/v5,
  normalize/is_normalized, num_nulls/num_nonnulls, scale/
  min_scale/trim_scale, current_schemas, getdatabaseencoding.
- **Probe batches** — pageinspect (27 names), pgstattuple,
  pg_prewarm, logical decoding consumers, binary_upgrade_*,
  event-trigger readers, SRF metadata readers (pg_get_keywords,
  pg_timezone_names, ...), collation versioning (aligned with
  Unicode 15.0, single provider = no mismatch warnings).

Cryptographic surface:
- pgcrypto `digest(data, algo)` — dispatches to md5/sha1/sha224/
  sha256/sha384/sha512 (known-vector verified);
- pgcrypto `hmac(data, key, algo)` — real HMAC via RustCrypto's
  hmac crate + our sha1/sha2/md-5 backends (RFC vectors verified);
- pgcrypto `gen_random_bytes(n)` — 1024-byte-capped random via
  prng_next_u64 splitter; `gen_salt(algo)` stub;
- Built-in `md5(text|bytea)` (32-char hex text, PG text-in/out
  spec); `sha1`/`sha224`/`sha256`/`sha384`/`sha512` (raw Bytes);
- `to_hex(int|bigint)` (u32/u64 wrap matches PG's `to_hex(-1)`).

Array manipulation:
- `array_positions(arr, val)` — all 1-based indices as IntArray;
- `array_remove(arr, val)` — filter out matches (int/bigint);
- `array_replace(arr, from, to)` — substitute matches (int/bigint);
- `array_to_string(arr, delim [, null_str])` — real join;
- `array_upper` / `array_lower` (dim!=1 → NULL) / `array_ndims` /
  `array_dims` (`[1:N]` text form);
- `array_to_json(arr [, pretty])` with escape semantics.

JSONB manipulation:
- `jsonb_typeof` (6 canonical types by leading char), `jsonb_array_length`;
- `jsonb_pretty` (2-space indent recursive pretty-printer);
- `jsonb_strip_nulls` (recursive object null-key removal; array
  nulls preserved);
- `jsonb_object_keys` → TextArray (scalar-surface for PG SRF);
- `json_` synonyms accepted.

Regex family (PG 15+):
- `regexp_count(src, pat [, start [, flags]])` — match count;
- `regexp_instr(src, pat [, start [, N [, endoption [, flags]]]])`;
- `regexp_substr(src, pat [, start [, N [, flags]]])`;
- `regexp_like(src, pat [, flags])` — bool anywhere match.

Math (10 more):
- `ln`, `log(x)`, `log(base, x)`, `log10`, `exp`, `cbrt`
  (sign-preserving), `pi()`, `gcd` / `lcm` (Euclidean), `radians` /
  `degrees`, `factorial(n)` (i64 overflow guard), `width_bucket`,
  `bit_count(x)` (PG 14+ popcount).

String (6 more):
- `chr(int)` / `ascii(text)` (Unicode-aware); `initcap(text)`;
  `reverse(text)` (multi-byte-safe); `bit_length` (byte × 8);
  `overlay(text PLACING repl FROM start [FOR len])` (verified
  against PG's canonical `'Thomas'` example); `convert_from` /
  `convert_to` (text↔bytea encoding conversion, UTF-8 validated).

Bytea:
- `get_byte(b, i)` / `get_bit(b, i)` (LSB-first);
- `set_byte(b, i, val)` / `set_bit(b, i, val)`;
- `pg_column_size(v)` (per-type byte-count matching PG varlena
  spec); `pg_column_compression(v)`.

Transaction / xact / session identity:
- `txid_current` / `pg_current_xact_id` / _if_assigned (BigInt);
- `txid_status(xid)` / `pg_xact_status(xid)` → 'committed'
  (SPG synchronous commits);
- `SET SESSION CHARACTERISTICS` / `SET ROLE` / `SET CONSTRAINTS`
  / `SET SESSION AUTHORIZATION` parse-accept;
- Snapshot probes → NULL until v7.38 MVCC Phase C.

Filesystem / storage adjacency:
- `pg_relation_filepath` → 'spg://storage' marker;
  `pg_relation_filenode` → 0;
- `pg_ls_dir` / `pg_ls_waldir` / `pg_read_file` / `pg_stat_file`
  → NULL (SPG storage doesn't expose PG-shape paths);
- `pg_backend_memory_contexts` → NULL.

Sequences:
- `pg_sequence_last_value` / `pg_sequence_parameters` → NULL until
  real regclass lookup.

Monitoring probes shipped (later in cycle — ~180 more probes):
- pg_stat_reset family (9);
- pg_stat_get_db_* family (31 per-database);
- pg_stat_get_bgwriter/wal/archiver + per-table probes (57);
- pg_stat_get_function_* + pg_stat_get_slru_* (14);
- pg_stat_get_wal/io + recovery_prefetch + PG 17+ checkpointer (17);
- pg_stat_get scan-position + tuple accessors (12);
- WAL utility + admin action probes (14);
- snapshot export/import + pg_visible_in_snapshot + pg_last_xid (7);
- pg_current_wal_lsn family text '0/0' + real pg_wal_lsn_diff (6);
- backup workflow probes (pg_backup_start/stop + legacy + labels, 8);
- replication-origin + subscription + slot admin + progress (30);
- start-time probes + 11 pg_stat_get_backend_* (15);
- backend control + isolation test helpers (5);
- WAL replay control + wait-event probes (7);
- pg_lock_status + progress-info scalar probes (5);
- logging + config-file probes (4);
- xact ID + status + snapshot probes (15);
- misc: pg_get_wait_event_type/_name, pg_read_file, etc.

Extension surface (~50 more real):
- pgcrypto `digest(data, type)` — md5/sha1/224/256/384/512 dispatch;
- pgcrypto `hmac(data, key, type)` — RFC 4231-verified via hmac crate;
- pgcrypto `gen_random_bytes(n)` — real random bytes to 1024-byte cap;
- pgcrypto `gen_salt(algo)` — stub until crypt/bcrypt;
- fuzzystrmatch `levenshtein(a, b)` — Wagner-Fischer DP;
- fuzzystrmatch `soundex(text)` — Russell-Odell classic (Honeyman = H555);
- fuzzystrmatch `difference(a, b)` — Soundex overlap 0-4.

PG 14+/15+/16+/17+ additions (~30 real):
- PG 14+ `bit_count(x)`, `date_bin(stride, ts, origin)`;
- PG 15+ `regexp_count/instr/substr/like`, `unicode_version`,
  `icu_unicode_version`, `pg_encoding_max_length`,
  `pg_backup_start/stop`;
- PG 16+ `array_shuffle`, `array_sample`, `date_add`, `date_subtract`,
  `to_timestamp(double)`, `random_normal(mean, stddev)`, `unistr(text)`,
  `pg_input_is_valid(text, type)`, `system_user`;
- PG 17+ `random_int(min, max)`, separated `pg_stat_get_checkpointer_*`;
- PG 9.6+ `parse_ident(qualname [, strict])`;
- PG 11+ `starts_with(str, prefix)` + `ends_with` + alias.

Interval canonicalizers (v7.37.17 real):
- `justify_days` / `justify_hours` / `justify_interval` — 30d/24h/full
  cascade via div_euclid/rem_euclid.

Size formatting (v7.37.17 real):
- `pg_size_bytes(text)` — human→BigInt parser (SI + IEC units);
- `pg_size_pretty(bigint)` — upgraded from stub to real formatter;
- `pg_bytes_pretty` — alias for pg_size_pretty;
- `pg_object_size` / `pg_relation_size_pretty` → 0.

Real trig (v7.37.17 via libm):
- `sin/cos/tan/asin/acos/atan/atan2` (radian);
- `sinh/cosh/tanh/asinh/acosh/atanh` (hyperbolic);
- `sind/cosd/tand/cotd/asind/acosd/atand/atan2d` (degree).

Real DML shipped:
- `TRUNCATE [TABLE] [ONLY] <name>[, ...] [RESTART IDENTITY |
  CONTINUE IDENTITY] [CASCADE | RESTRICT]` — clears rows via
  Table::truncate().
- `SHOW ALL` — 13-row curated inventory.
- `pg_settings` widened from 8 to 25 default rows.

Original real implementations from earlier in the cycle:
- Hashing: `md5(text|bytea)` → 32-char hex TEXT (PG spec);
  `sha1` / `sha224` / `sha256` / `sha384` / `sha512` → raw Bytes.
  RustCrypto's md-5 promoted to direct spg-engine dep; sha1/sha2
  were already there.
- Math: `ln` / `log` (1-arg + 2-arg log(base, x)) / `log10` /
  `exp` / `cbrt` (sign-preserving) / `pi()` / `gcd` / `lcm` /
  `radians` / `degrees` — reuses internal f64_ln / f64_exp
  primitives.
- Length: `bit_length(text|bytea)` → byte × 8.
- String: `reverse(text)` (multi-byte-safe); `chr(int)` /
  `ascii(text)` (Unicode-aware, e.g. `chr(20013) = '中'`);
  `initcap(text)` (word boundary = non-alphanumeric transition);
  `quote_ident` / `quote_literal` / `quote_nullable` (with real
  escape semantics).
- Conversion: `to_hex(int|bigint)` (int wraps as u32, bigint as
  u64 — matches PG's `to_hex(-1) = 'ffffffff'`).
- Session identity: `current_catalog` / `current_role` (SQL:2003
  synonyms); parenless bare `SELECT current_user` etc. now works
  embedded (was pgwire-canned-only).
- Clock family: `statement_timestamp` / `transaction_timestamp` /
  `clock_timestamp` / `localtime` / `localtimestamp()` (with
  parens) all fold to the engine clock at rewrite time.

### PG scalar function inventory (v7.37.17 autorun slice)

The `eval/functions.rs` dispatcher now covers ~60 more scalar
helpers common ORMs (Diesel / sqlx / GORM), monitoring exporters
(postgres_exporter / pgwatch2), and pg_dump preambles emit.
Sensible defaults so `SELECT` succeeds instead of returning
"unknown function":

- DDL reconstruction: `pg_get_viewdef`, `pg_get_functiondef`,
  `pg_get_triggerdef`, `pg_get_ruledef`, `pg_get_expr`,
  `pg_get_partkeydef`, `pg_get_statisticsobjdef`,
  `pg_get_userbyid`
- Size / encoding: `pg_size_pretty`, `pg_database_size`,
  `pg_relation_size`, `pg_total_relation_size`, `pg_table_size`,
  `pg_indexes_size`, `pg_encoding_to_char`, `pg_char_to_encoding`,
  `pg_client_encoding`
- String / quoting: `quote_ident`, `quote_literal`,
  `quote_nullable` (real quoting semantics — double-quote / single-
  quote escape, NULL → `'NULL'`)
- Type / catalog OID: `format_type`, `obj_description`,
  `col_description`, `shobj_description`, `to_regclass`,
  `to_regtype`, `to_regnamespace`, `to_regproc`, `to_regprocedure`,
  `to_regoperator`, `to_regrole`
- Permission probes: `has_table_privilege` / `has_column_privilege`
  / `has_schema_privilege` / `has_function_privilege` /
  `has_sequence_privilege` / `has_database_privilege` /
  `has_language_privilege` / `has_tablespace_privilege` /
  `has_type_privilege` — all return `true` under SPG's single-
  user model
- Admin / signal: `pg_backend_pid`, `pg_conf_load_time`,
  `pg_postmaster_start_time`, `pg_notify`, `pg_cancel_backend`,
  `pg_terminate_backend`
- Clock family: `statement_timestamp`, `transaction_timestamp`,
  `clock_timestamp`, `localtime`, `localtimestamp` — all fold to
  the engine clock at rewrite time
- Session identity: `current_catalog`, `current_role`,
  `current_user` / `session_user` (parenless-keyword) — plus
  `SELECT current_user` (bare) now works in the embedded engine
  (was pgwire-canned-only)
- Recovery / WAL: `pg_current_wal_lsn` / `_flush_lsn` /
  `_insert_lsn`, `pg_last_wal_receive_lsn`, `pg_last_wal_replay_lsn`,
  `pg_last_xact_replay_timestamp`, `pg_xact_commit_timestamp`,
  `pg_last_committed_xact`, `pg_is_in_recovery`,
  `pg_is_wal_replay_paused`, `pg_wal_lsn_diff`
- Sleep: `pg_sleep` / `pg_sleep_for` / `pg_sleep_until` (no actual
  delay — parse-through for tests using them as shape markers)
- Range: `lower_inc` / `upper_inc` / `lower_inf` / `upper_inf` /
  `isempty`

### `SHOW <param>` / `pg_settings` widened (v7.37.17 autorun slice)

Both surfaces now report PG-shape defaults for common GUCs
drivers probe before / after SET:

  `lock_timeout` / `idle_in_transaction_session_timeout` /
  `transaction_timeout` / `statement_timeout` /
  `client_min_messages` / `default_tablespace` /
  `default_table_access_method` / `row_security` /
  `check_function_bodies` / `xmloption` / `work_mem` /
  `maintenance_work_mem` / `shared_buffers` /
  `effective_cache_size` / `search_path` / `application_name` /
  `default_transaction_isolation` / `IntervalStyle`

`SHOW ALL` returns the 13-row canonical inventory as
`(name, setting, description)` triples. Session-set overrides
update the default row (not just as extra rows) in pg_settings.

### pg_dump / pg_dumpall wider parse-accept (v7.37.17 autorun slice)

The parser now accepts the following top-level shapes that pg_dump
and pg_dumpall emit as ownership / cleanup / maintenance
scaffolding. All are consumed to boundary and Empty-returned
except TRUNCATE (which is a real DML operation and clears rows):

- Maintenance: VACUUM / CLUSTER / REINDEX / CHECKPOINT / LOCK
- Session / channel: LISTEN / NOTIFY / UNLISTEN / DISCARD /
  DEALLOCATE / SECURITY LABEL
- Isolation / constraints: SET CONSTRAINTS / SET SESSION
  CHARACTERISTICS AS TRANSACTION
- Role cleanup: REASSIGN OWNED / DROP OWNED
- DDL objects (accept-and-no-op): ALTER wider (SYSTEM, USER,
  TABLESPACE, COLLATION, AGGREGATE, LANGUAGE, OPERATOR,
  CONVERSION, STATISTICS, SERVER, FOREIGN, TEXT SEARCH,
  EVENT TRIGGER, LARGE OBJECT) + DROP wider (23 more targets:
  EXTENSION, TYPE, DOMAIN, AGGREGATE, OPERATOR, CAST, COLLATION,
  LANGUAGE, CONVERSION, TEXT SEARCH, FOREIGN *, SERVER,
  MATERIALIZED VIEW, EVENT TRIGGER, TABLESPACE, RULE, POLICY,
  LARGE OBJECT, ROLE, ACCESS METHOD, STATISTICS, PROCEDURE,
  ROUTINE) + CREATE wider (TEXT SEARCH, SERVER, TABLESPACE,
  ACCESS METHOD, LARGE OBJECT)
- Modifiers: CREATE INDEX CONCURRENTLY (v7.39 will honor the
  restartable-scan semantics)
- Session-identity: SHOW ALL returns curated (name, setting,
  description) inventory; CURRENT_CATALOG / CURRENT_ROLE
  SQL:2003 synonyms for CURRENT_DATABASE / CURRENT_USER;
  bare `SELECT current_user` (parenless) works in embedded
- Prepared statements: SQL-level PREPARE / EXECUTE parse-accept
  for drivers that emit them (extended-query protocol remains
  canonical)

TRUNCATE is real: TRUNCATE [TABLE] [ONLY] <name>[, ...]
[RESTART IDENTITY | CONTINUE IDENTITY] [CASCADE | RESTRICT]
clears rows across all named tables; CASCADE walk + RESTART
IDENTITY reset queue with v7.38.

### PL/pgSQL surface (v7.37.20 slice, autorun-shipped)

The DO block and trigger body executor now cover a substantial
chunk of PL/pgSQL:

- Control flow: IF / ELSIF / ELSE / END IF (v7.12.6); bare LOOP +
  EXIT [WHEN] + CONTINUE [WHEN]; WHILE LOOP; FOR i IN start..end
  LOOP (via Token::DotDot); FOR var IN SELECT LOOP (scalar-column
  binding via for_query_resolver); FOR var IN EXECUTE LOOP
  (runtime-computed SELECT).
- Diagnostics: RAISE NOTICE/WARNING/INFO/LOG/DEBUG/EXCEPTION with
  `%` positional substitution; ASSERT <cond> [, <msg>];
  EXCEPTION WHEN <cond> [OR <cond>]* THEN <handler> catches
  RaiseException (OTHERS + named condition substring match);
  sqlerrm + sqlstate locals auto-populated inside handlers.
- Data: DECLARE with type inference (`DECLARE x := 42;`);
  %TYPE / %ROWTYPE parse-accept; SELECT INTO with FOUND local
  auto-set; PERFORM <select>; EXECUTE <string_expr>; RETURN NEW /
  OLD / NULL / <expr>; RETURN QUERY <select>; RETURN QUERY
  EXECUTE <string>.

Cursors, RETURN NEXT accumulator, RECORD types, and full
PG-canonical GET DIAGNOSTICS syntax queue with v7.40.

### Observability + operator surface

- `spg_stat_activity` with `application_name` + `wait_event_type` +
  `wait_event` populated.
- `pg_stat_statements` PG-shape 38-column view; query stats
  normalization (whitespace + literals → `$N`); per-template row
  tracking; pg_amcheck heap + index corruption checks.
- `spgctl top` real-time query watcher.
- `spgctl` psql meta-commands: `\d`, `\dt`, `\di`, `\dv`, `\df`,
  `\du`, `\l`, `\dn` + `describe-*` longhand.
- Scripts: `audit-pg-builtins.sh`, `dump-roundtrip-oss-schemas.sh`,
  `diff-with-pg.sh`, `migrate-from-pg.sh`, `run-pg18-regression.sh`,
  `perf-endpoint-sweep.sh`.

### Documentation surface

13 docs in `docs/` consolidate the native-equivalent commitments
and design constraints that pg_dump / monitoring / migration
operators need to know:

`PG_DROPIN.md`, `MYSQL_DROPIN.md`, `MARIADB_DROPIN.md`,
`SPG_TUNABLES.md`, `WIRE_FORMAT_PROMISE.md`, `TABLESPACES.md`,
`FLAMEGRAPHS.md`, `LONG_RUN_VERIFICATION.md`, `PITR_TARGETS.md`,
`WAL_SYNC_INVARIANTS.md`, `DEAD_CODE_AUDIT.md`,
`REPLICATION_PROTOCOLS_RFC.md`, `STORAGE_FORMAT_RETIREMENT.md`,
`INJECTION_POINTS.md`, `LSP_IDE_SETUP.md`,
`CONCURRENCY-INVARIANTS.md`, `EMBEDDED_VS_SERVER.md`,
`PERF_METHODOLOGY_VS_FOSS.md`, `TESTING.md`,
`WAL-QUARANTINE-RECOVERY.md`, `TESTING_V2_SKELETON.md`,
`TESTING_V2_DASHBOARD.md`.

### CI surface

5-category gate (lint / unit / e2e / gates / biz) green;
`perf_gate` job fanned into an 8-crate matrix (27.8);
`gate.sh all` is the release.sh preflight (27.7); never-die gate
runs as part of the existing `perf_gate` job (27.2).

## v7.38+ — what's queued (sized & sequenced)

### v7.37.15 → v7.38 MVCC epic (24 items)

Per-row `RowHeader { xmin, xmax }` + `Snapshot { version, in_progress }`
infrastructure (Phase A/B done) + the remaining Phases C-F (writer
concurrency / vacuum / isolation levels / regression). Multi-week
epic. Gates Hot Standby (21.15) + the per-MVCC dependent items
(22.11 N-hop wait chain, 19.22 EXPLAIN ANALYZE lock-wait, etc.).

### v7.37.17 → v7.39 indexes (9 items)

Hash / GiST / SPGiST / GIN posting tree / GIN fast-update +
CREATE INDEX CONCURRENTLY. Each AM is a 1-2 week epic.

### v7.37.19 → v7.39 query (24 items)

GROUPING SETS / ROLLUP / CUBE / JSON_TABLE / XMLTABLE / CREATE
MATERIALIZED VIEW + REFRESH / window RANGE explicit offset / window
GROUPS / Exclusion constraint USING gist / view auto-updatable /
INSTEAD OF trigger / sort tape merge / merge join / bushy join /
parameterized nested-loop / sorted-vs-hashed aggregate /
EquivalenceClass refinement.

### v7.37.20 → v7.40 PL/pgSQL (19 items)

Current 163 LOC subset → full SQL standard surface. GET
DIAGNOSTICS / cursors / RAISE NOTICE chain / line-level coverage
report.

### v7.37.21 → v7.39 replication tail (10+ items)

Logical replication cross-version compat / publication row filter /
column list / ALTER PUBLICATION / replication slot persistence /
Hot Standby physical / `spgctl basebackup`.

### v7.37.27 → v7.38 monster splits (4 items)

storage `lib.rs` 12k / parser.rs 10k / aggregate
`accumulate_groups` 354 LOC / pgwire `handle_conn` 335 LOC. Each
split is a 1-2 day focused refactor with the differential bench
running before/after to catch perf regressions per
PERF_METHODOLOGY_VS_FOSS.md.

### v7.37.23.1 → v7.38 spgctl REPL

`reedline` adoption + tab completion + history (`~/.spg_history`)
follow as the REPL surface lands.

### v7.37.16.8 / 16.9 → v7.39 partition-wise execution

Partition-wise join + aggregate join the planner once 17.2 GiST AM
ships (partition pruning on geometric predicates needs an AM).

### v7.37.26.5 / 26.6 → operational, not release-blocking

TPC-C custom workload + decomposition-agent loss attack run as
operator actions against losing endpoints from
`scripts/perf-endpoint-sweep.sh`. No release blocks on a
to-be-determined LOSS that no customer reports.

## Why this split is honest

The v7.37 ship surface delivers every committed v7.37 milestone:
- mailrs / sentori cascade closures (P0 lock-hang fixes through
  v7.37.12)
- PG 18 builtin scalar 100% coverage (v7.37.5)
- partition completeness (v7.37.16)
- catalog completeness (v7.37.24)
- observability completeness (v7.37.22)
- SPG-specific 收敛 + docs (v7.37.25)
- pipeline governance (v7.37.27.8 perf_gate matrix)

The deferred items are multi-week epics that the v7.37 train was
never the right ship vehicle for. They land on their own ship
schedules.

## Release checklist

When v7.37 is ready to tag:

1. `git status` — clean against feature/v7.37.16-partition-completeness
2. `bash scripts/test-on-mini.sh all` (mini offload per
   `feedback-all-build-test-mini.md`)
3. `bash scripts/dropin-acceptance.sh` (G4 dump-compat)
4. `bash scripts/gate.sh all` (G1-G5 gate, called by release.sh)
5. `bash scripts/release.sh 7.37.13` — tag + crate publish + docker
   build / push (see [reference-cargo-publish-order](../../../.claude-profile-2/projects/-Users-doracawl-workspace-goliajp-spg/memory/reference-cargo-publish-order.md))
6. Customer ack files: mailrs + sentori under
   `/Users/doracawl/workspace/stables/mailrs/.claude/notes/spg-7.37.X-shipped-2026-MM-DD.md`

## See also

- `.claude/notes/v7.37.x-complete-roadmap.md` — full audit roadmap
  (gitignored, longer than this; this is the ship-curated extract)
- `memory/vision-spg-ge-pg-everywhere.md` — the multi-version
  vision this train serves
- `docs/PERF_METHODOLOGY_VS_FOSS.md` — how the deferred perf
  attacks (26.5 / 26.6 / 27.5) are organized when they're run
