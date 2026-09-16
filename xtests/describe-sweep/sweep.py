# The type Describe announces for 189 common expressions, against PostgreSQL.
#
#   sweep.py <pg-uri> <spg-uri>     exit 0 = only recorded divergences,
#                                   1 = a new one, 2 = the sweep could not run
#
# 8.0.3 — a release gate (`write-arbitration` runs it beside the arbiter
# sweep). A driver decodes a column by the announced type; in binary format a
# wrong one is a wrong answer, silently. 57 of these differed on 8.0.2.
import os, subprocess, sys
PSQL = os.environ.get("PSQL", "psql")
EXPRS = r"""
'abc' ~ 'b'
'abc' ~* 'B'
'abc' !~ 'b'
'abc' SIMILAR TO 'a%'
'abc' LIKE 'a%'
'abc' ILIKE 'A%'
regexp_like('abc','b')
starts_with('abc','a')
'abc' ^@ 'a'
extract(epoch from now())
extract(year from now())
date_part('year', now())
EXTRACT(day FROM date '2026-01-01')
now()
current_date
current_time
localtimestamp
clock_timestamp()
statement_timestamp()
age(timestamp '2026-01-01')
date_trunc('day', now())
to_char(now(), 'YYYY')
to_timestamp(0)
to_date('2026-01-01','YYYY-MM-DD')
make_date(2026,1,1)
make_interval(days => 1)
justify_days(interval '35 days')
length('abc')
char_length('abc')
octet_length('abc')
bit_length('abc')
position('b' in 'abc')
strpos('abc','b')
upper('a')
lower('A')
initcap('ab')
substring('abc' from 2)
substr('abc',2)
left('abc',2)
right('abc',2)
lpad('a',3)
rpad('a',3)
trim(' a ')
btrim(' a ')
replace('abc','b','x')
translate('abc','b','x')
reverse('abc')
repeat('a',3)
split_part('a,b',',',1)
concat('a','b')
concat_ws(',','a','b')
format('%s','a')
quote_ident('a')
quote_literal('a')
md5('a')
sha256('a'::bytea)
encode('a'::bytea,'hex')
decode('61','hex')
ascii('a')
chr(97)
regexp_replace('abc','b','x')
regexp_matches('abc','b')
regexp_split_to_array('a,b',',')
regexp_count('abcb','b')
regexp_instr('abc','b')
regexp_substr('abc','b')
string_to_array('a,b',',')
array_to_string(ARRAY[1,2],',')
array_length(ARRAY[1,2],1)
cardinality(ARRAY[1,2])
array_position(ARRAY[1,2],2)
ARRAY[1,2] @> ARRAY[1]
ARRAY[1,2] && ARRAY[1]
1 = ANY(ARRAY[1,2])
abs(-1)
abs(-1.5)
ceil(1.5)
floor(1.5)
round(1.55,1)
round(1.5)
trunc(1.5)
sqrt(2)
cbrt(8)
power(2,3)
mod(7,3)
7 % 3
7 / 2
7.0 / 2
div(7,2)
sign(-2)
exp(1)
ln(2)
log(100)
log10(100)
pi()
random()
greatest(1,2)
least(1,2)
coalesce(null,1)
nullif(1,2)
width_bucket(5,0,10,5)
gcd(12,8)
lcm(4,6)
factorial(5)
1::numeric
'1'::int
1 + 1.5
1 || 'a'
'a' || 'b'
now() - interval '1 day'
now() - now()
date '2026-01-02' - date '2026-01-01'
now()::date
to_json('a'::text)
to_jsonb('a'::text)
json_build_object('a',1)
jsonb_build_object('a',1)
'{"a":1}'::jsonb -> 'a'
'{"a":1}'::jsonb ->> 'a'
'{"a":1}'::jsonb ? 'a'
'{"a":1}'::jsonb @> '{"a":1}'
jsonb_typeof('1'::jsonb)
jsonb_array_length('[1]'::jsonb)
json_array_length('[1]'::json)
jsonb_path_exists('{"a":1}'::jsonb,'$.a')
gen_random_uuid()
uuid_generate_v4()
pg_typeof(1)
version()
current_user
session_user
current_database()
current_schema()
txid_current()
pg_backend_pid()
inet_client_addr()
count(*)
sum(1)
sum(1.5)
avg(1)
min(1)
max('a')
bool_and(true)
bool_or(true)
string_agg('a',',')
array_agg(1)
json_agg(1)
jsonb_agg(1)
stddev(1)
variance(1)
percentile_cont(0.5) WITHIN GROUP (ORDER BY 1)
row_number() OVER ()
rank() OVER ()
dense_rank() OVER ()
ntile(2) OVER ()
lag(1) OVER ()
first_value(1) OVER ()
percent_rank() OVER ()
cume_dist() OVER ()
EXISTS (SELECT 1)
1 IN (1,2)
1 BETWEEN 0 AND 2
true AND false
NOT true
1 IS NULL
1 IS DISTINCT FROM 2
'a' IS NOT NULL
CASE WHEN true THEN 1 ELSE 2 END
CASE WHEN true THEN 'a' END
nextval('seq_gd')
to_tsvector('a b')
to_tsquery('a')
to_tsvector('a') @@ to_tsquery('a')
ts_rank(to_tsvector('a'), to_tsquery('a'))
similarity('abc','abd')
point(1,2)
box(point(0,0),point(1,1))
int4range(1,3)
int4range(1,3) @> 2
lower(int4range(1,3))
isempty(int4range(1,1))
inet '1.2.3.4'
'1.2.3.4'::inet << '1.2.3.0/24'::inet
host('1.2.3.4'::inet)
masklen('1.2.3.0/24'::inet)
B'101'
'101'::bit(3) & '110'::bit(3)
get_bit('101'::bit(3),0)
xmin
"""
EXPRS = [e.strip() for e in EXPRS.strip().splitlines() if e.strip()]

# Recorded, not fixed: two extension functions PostgreSQL's test image does
# not install, and seven types SPG's value model does not carry.
RECORDED = {
    "uuid_generate_v4()", "similarity('abc','abd')",
    "pg_typeof(1)", "inet_client_addr()", "point(1,2)", "box(point(0,0),point(1,1))",
    "int4range(1,3)", "B'101'", "xmin",
}

def per_expr(uri):
    res = []
    for e in EXPRS:
        if e == "xmin":
            q = "SELECT xmin AS c FROM gd_t \\gdesc"
        else:
            q = f"SELECT {e} AS c \\gdesc"
        r = subprocess.run([PSQL, uri, "-X", "-A", "-t"], input=q, capture_output=True, text=True, timeout=60)
        if "|" in r.stdout:
            res.append(r.stdout.strip().split("|", 1)[1])
        elif r.stderr.strip():
            res.append("ERR: " + r.stderr.strip().splitlines()[0][:60])
        else:
            res.append("?")
    return res


setup = "CREATE SEQUENCE IF NOT EXISTS seq_gd; CREATE TABLE IF NOT EXISTS gd_t (a int);"
PG, SPG = sys.argv[1], sys.argv[2]
for u in (PG, SPG):
    subprocess.run([PSQL, u, "-X", "-q", "-c", setup], capture_output=True)
a = per_expr(PG)
b = per_expr(SPG)
# A sweep whose oracle answered nothing measured nothing.
if len(EXPRS) < 150 or sum(1 for x in a if x.startswith("ERR") or x == "?") > 10:
    print(f"the sweep could not run: {len(EXPRS)} expressions, PG answered {len(a)}")
    sys.exit(2)
new = 0
for e, x, y in zip(EXPRS, a, b):
    if x != y:
        tag = "recorded" if e in RECORDED else "NEW"
        new += tag == "NEW"
        print(f"{tag:8s} {e:55s} PG={x:30s} SPG={y}")
print(f"exprs={len(EXPRS)} new={new}")
sys.exit(1 if new else 0)
