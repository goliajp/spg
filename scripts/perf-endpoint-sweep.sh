#!/usr/bin/env bash
# perf endpoint sweep — SPGS against PG18, one client, one window.
#
# Round 885 rewrote this. What it used to do, and why each part had to
# change, is worth keeping because the old shape is the shape a perf
# harness naturally grows into:
#
#   * It drove SPG with `spgctl query` and PG with `psql`, then compared
#     wall clock. That is `docs/BENCH_PROTOCOL.md` rule 1, and the rule
#     exists because the violation once reported SPG at 2.1x slower when
#     the real figure through one client was 1.15-1.35x — about 0.35 s of
#     the "loss" was the probe. The panel was measuring two CLIENTS.
#     SPG speaks the PG wire protocol, so both legs run psql now.
#
#   * It judged on the ratio of MEDIANS at a 1.05x threshold. The same
#     testbed has produced 8-23 differing cells from two runs of the SAME
#     binary (v7.37 audit, r648-r650), so a 1.05x median threshold
#     manufactures losses that no change can fix. Verdicts are now
#     non-overlapping ranges — `BENCH_PROTOCOL.md` rule 4 — and every
#     CELL carries a same-binary CONTROL leg, timed between the other
#     two. Where the binary separates from itself, the cell's verdict is
#     WITHDRAWN and reported `unresolved`: the panel does not get to
#     call something equal when it cannot tell, and it does not get to
#     call something a loss either.
#
#     v7.38.18 — the two sentences above used to describe the control as
#     the run's resolution while the code consulted it for nothing. It
#     ran at one size, after every size had finished, and its count was
#     printed and dropped. A prerelease run called `two keys` at 1,000
#     rows a LOSS on a 27 microsecond separation; its control, taken
#     later in a calmer window, reported a clean floor. The same cell
#     re-measured on a quiet box at N=25 was 0.524-0.645 against PG's
#     0.516-0.718 — no gap. A header that promised a check the code did
#     not perform is the same defect as a compatibility table nobody
#     re-ran.
#
#   * It covered the dogfood corpus only. The whole ORDER BY surface was
#     outside it, which is how 29 losing cells out of 32 went unreported
#     until a one-off sweep found them (see
#     `.claude/state/orderby-perf-design-2026-08-09.md`). The shapes are
#     built in below and run at four sizes.
#
# Inputs:
#   PG_URI      — postgres://... for the PG18 leg (required)
#   SPG_URI     — postgres://... for the SPGS leg (required)
#   N           — timings per side per cell (default 5; rule 4 wants >= 3,
#                 and 3 has proved too few to separate 10% at this size)
#   SIZES       — row counts for the built-in shapes (default "1000 10000 50000 400000")
#
#   N           — see above; a cell's control leg costs a third of the
#                 run's timings and is not optional.
#
# Exit 0 when no cell LOSES beyond its own control's resolution; 1
# otherwise. The verdict line carries `withdrawn=` — the number of
# win/LOSS calls the control took back — so a summary cannot report a
# clean sweep without saying how much of it was unreadable.
set -euo pipefail
cd "$(dirname "$0")/.."

PG_URI="${PG_URI:-}"
SPG_URI="${SPG_URI:-}"
N="${N:-5}"
SIZES="${SIZES:-1000 10000 50000 400000}"
PSQL="${PSQL:-psql}"

[[ -n "${PG_URI}" ]]  || { echo "fatal: PG_URI must be set" >&2; exit 2; }
[[ -n "${SPG_URI}" ]] || { echo "fatal: SPG_URI must be set (both legs run psql — rule 1)" >&2; exit 2; }

echo "load before: $(uptime)"
# v7.38.19 — say which collation each leg is serving.
#
# The testbed exports LANG and LC_ALL, so the leg the gate called its `C`
# baseline had been running under en_US for as long as the gate has run
# on it, and nothing printed said so. A panel comparing "locale against
# C" was comparing en_US against en_US and reporting no losses. A number
# whose conditions are not printed is a number nobody can check.
#
# And CHECKED, not only printed. Three times in this version an
# instrument printed something true and nothing compared it to what was
# expected, so the expectation is stated here: `EXPECT_SPG_COLLATE`
# defaults to `C` because that is what every cell's history was measured
# under, and a leg serving something else is a run whose numbers are not
# comparable to the ones before it.
EXPECT_SPG_COLLATE="${EXPECT_SPG_COLLATE:-C}"
leg_collation() {
  "${PSQL}" --no-psqlrc -X -q -t -A "$1" \
    -c 'SELECT datcollate FROM pg_database LIMIT 1' 2>/dev/null | head -1
}
spg_coll="$(leg_collation "${SPG_URI}")"
pg_coll="$(leg_collation "${PG_URI}")"
echo "leg SPGS collation: ${spg_coll:-<unknown>}   leg PG18 collation: ${pg_coll:-<unknown>}"
if [[ "${spg_coll}" != "${EXPECT_SPG_COLLATE}" ]]; then
  echo "fatal: the SPGS leg serves collation ${spg_coll:-<unknown>}, expected ${EXPECT_SPG_COLLATE}." >&2
  echo "       Every cell's history was measured under ${EXPECT_SPG_COLLATE}; a leg serving" >&2
  echo "       something else produces numbers that cannot be compared to them. Pass" >&2
  echo "       EXPECT_SPG_COLLATE to say you meant it." >&2
  exit 2
fi

# r1022 — the two legs must reach the client over the SAME route, and this
# refuses to score them when the URIs say they do not.
#
# It was scored with them unequal. `PG_URI` pointed at 127.0.0.1 inside the
# oracle container while `SPG_URI` went out to `host.docker.internal`, so
# every SPG statement paid a container-to-host hop that no PG statement
# paid. Measured on that testbed: PG's own `SELECT 1` is 0.011 ms over the
# in-container loopback and 0.177 ms over the crossing — 0.166 ms handed to
# one side of every cell, which sub-millisecond cells are entirely made of.
#
# The verdicts it produced: 20 of 32 cells losing. With both legs at
# host.docker.internal, on the same binaries in the same hour: 2. A whole
# campaign's headline, and it was the harness.
#
# The check is on the URIs and not on a measurement, because the measurement
# cannot tell the two apart. A table-free `SELECT 1` costs the path AND the
# engine's own per-statement work, and those are not separable from outside:
# over one route this pair reads 0.094 ms on SPGS against 0.175 on PG18 —
# an honest 1.9x that says nothing about the network. A first version of
# this gate compared those floors and refused a correctly-configured run.
host_of() { # postgres://user[:pw]@HOST:port/db -> HOST
  printf '%s\n' "$1" | sed -E 's|^[a-z+]*://||; s|^[^@]*@||; s|[:/].*$||'
}
pg_host=$(host_of "${PG_URI}")
spg_host=$(host_of "${SPG_URI}")
echo "route: SPGS via ${spg_host}, PG18 via ${pg_host}"
if [[ "$pg_host" != "$spg_host" && "${ALLOW_ASYMMETRIC_HOSTS:-0}" != 1 ]]; then
    cat >&2 <<ROUTE
fatal: the two legs are reached over different hosts.
       PG18 via '${pg_host}', SPGS via '${spg_host}'.
       A container-to-host hop costs ~0.17 ms on this testbed, which is
       more than several cells below take in total, and it would be
       charged to one engine and not the other. Point both URIs through
       the same route and run again.
       If they genuinely are equidistant, set ALLOW_ASYMMETRIC_HOSTS=1.
ROUTE
    exit 2
fi

time_one() { # $1=uri $2=sql $3=work_mem setting
  "${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "$3" -c '\timing on' -c "$2" -c "$2" -c "$2" 2>&1 |
    grep -E '^Time:' | sed 's/Time: //; s/ ms//' | sort -g | head -1
}
lo() { printf '%s\n' "$@" | sort -g | head -1; }
hi() { printf '%s\n' "$@" | sort -g | tail -1; }

# a-range strictly above b-range => LOSS; strictly below => win; else the
# panel cannot tell them apart at this resolution.
verdict() { # $1=amin $2=amax $3=bmin $4=bmax
  awk -v amin="$1" -v amax="$2" -v bmin="$3" -v bmax="$4" \
    'BEGIN { if (amin > bmax) print "LOSS"; else if (amax < bmin) print "win"; else print "unresolved" }'
}

setup_table() { # $1=uri $2=table $3=rows $4=work_mem-setting
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "DROP TABLE IF EXISTS $2" \
    -c "CREATE TABLE $2 (id INT PRIMARY KEY, k INT, pad TEXT)" >/dev/null 2>&1
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "INSERT INTO $2 SELECT g, ((g::bigint*7919)%$3)::int, repeat(chr(97+(g%26)),200) FROM generate_series(1,$3) g" >/dev/null 2>&1
  local got
  got="$("${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "SELECT count(*) FROM $2")"
  # Rule 2: a timing read off an unverified table is not evidence.
  [[ "${got}" == "$3" ]] || { echo "SETUP FAILED: $2 on $1 has ${got} rows, wanted $3 — refusing to time" >&2; exit 2; }
}

# r1042 — a SECOND fixture, for the types v7.37.26 gave index keys to.
#
# It is separate rather than three more columns on the first one because
# widening that row would move every existing cell's timing, and the
# panel's value is that a number means the same thing it meant last
# release.
#
# `n` takes a thousand distinct two-decimal values, each repeating
# `rows/1000` times: two decimals is the shape a money or a rate column
# has, and the repeats give `DISTINCT` real work instead of handing it a
# permutation. `b` is the row's id as eight bytes, so byte order and
# numeric order agree — a wrong comparator then shows up as a wrong
# ORDER BY rather than as nothing at all.
setup_typed_table() { # $1=uri $2=table $3=rows
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "DROP TABLE IF EXISTS $2" \
    -c "CREATE TABLE $2 (id INT PRIMARY KEY, n NUMERIC, b BYTEA, pad TEXT)" >/dev/null 2>&1
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "INSERT INTO $2 SELECT g, (((g::bigint*7919)%1000)::numeric)/100, decode(lpad(to_hex(g), 16, '0'), 'hex'), repeat(chr(97+(g%26)),200) FROM generate_series(1,$3) g" >/dev/null 2>&1
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "CREATE INDEX ${2}_n ON $2 (n)" \
    -c "CREATE INDEX ${2}_b ON $2 (b)" >/dev/null 2>&1
  local got
  got="$("${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "SELECT count(*) FROM $2")"
  [[ "${got}" == "$3" ]] || { echo "SETUP FAILED: $2 on $1 has ${got} rows, wanted $3 — refusing to time" >&2; exit 2; }
}

# Rule 2 again, one level deeper: a row count says the table is there and
# says nothing about whether the PREDICATES below select anything. A
# literal that matches no row times an empty answer, and an empty answer
# is fast on both engines and means nothing.
#
# Round 1025 shipped a fixture whose `g*7919` overflowed int4, so three
# cells timed an error response — three nearly identical numbers that
# should have been questioned on sight. This asks each typed predicate
# for its count before anything is timed, and refuses a zero.
verify_typed_predicates() { # $1=uri $2=table
  local q c
  for q in "SELECT count(*) FROM $2 WHERE n = 1.23" \
           "SELECT count(*) FROM $2 WHERE n BETWEEN 1 AND 2" \
           "SELECT count(*) FROM $2 WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')" \
           "SELECT count(DISTINCT n) FROM $2"; do
    c="$("${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "${q}")"
    [[ "${c}" =~ ^[0-9]+$ && "${c}" -gt 0 ]] || {
      echo "SETUP FAILED on $1: [${q}] answered '${c}' — refusing to time a predicate that selects nothing" >&2
      exit 2
    }
  done
}

SPG_WM='SET work_mem = 4096'
PG_WM="SET work_mem='4MB'"

# v7.38.19 — a THIRD fixture, for the sort panel, and it is separate for
# the same reason the typed one is: widening `sweep_N` would move every
# existing cell's timing, and the panel's value is that a number means
# what it meant last time.
#
# What it is for: `pad` is `repeat(chr(97+(g%26)), 200)` — twenty-six
# distinct values, each two hundred identical characters. A sort over it
# is decided by the first byte, and any key that looks at a prefix would
# look spectacular on it while proving nothing about a real workload.
# It is also two hundred bytes, so it exercises only the heap arm of the
# sort key and never the inline one.
#
# So this table carries text that is DISTINCT per row, in both regimes:
#
#   s_short   nine bytes  — fits a sort key inline
#   s_long    ~192 bytes  — does not, and no two rows share a prefix
#
# A sort attack judged on `pad` alone is a sort attack judged on a
# degenerate case.
setup_sort_table() { # $1=uri $2=table $3=rows
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "DROP TABLE IF EXISTS $2" \
    -c "CREATE TABLE $2 (id INT PRIMARY KEY, s_short TEXT, s_long TEXT)" >/dev/null 2>&1
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "INSERT INTO $2 SELECT g,
          'k' || lpad(((g::bigint*7919)%$3)::text, 8, '0'),
          md5(g::text) || md5((g*3)::text) || md5((g*7)::text) ||
          md5((g*11)::text) || md5((g*13)::text) || md5((g*17)::text)
        FROM generate_series(1,$3) g" >/dev/null 2>&1
  local got
  got="$("${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "SELECT count(*) FROM $2")"
  [[ "${got}" == "$3" ]] || { echo "SETUP FAILED: $2 on $1 has ${got} rows, wanted $3 — refusing to time" >&2; exit 2; }
  # Rule 2 again, one level down: a fixture built to be VARIED that turns
  # out not to be is worse than the one it replaced, because the panel
  # would then claim to cover a case it does not.
  local distinct
  distinct="$("${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "SELECT count(DISTINCT s_long) FROM $2")"
  [[ "${distinct}" == "$3" ]] || { echo "SETUP FAILED: $2 on $1 has ${distinct} distinct s_long, wanted $3 — the fixture is not varied" >&2; exit 2; }
}

# v7.38.19 — the SORT panel, verdicted separately from the shapes above.
#
# Every ORDER BY shape in `SHAPES` returns its rows, and at 400,000 rows
# of TEXT the wire transfer dominates the cell. SPG's encoder is fast
# enough to win them all while the sort inside is 1.2-2x behind
# PostgreSQL -- measured by wrapping the same work in `count(*)`, which
# returns one row and leaves only the sort:
#
#   ORDER BY a text column   SPG 52.2 ms   PG 42.4   1.23x
#   ORDER BY an int column   SPG 29.9 ms   PG 14.6   2.05x
#
# And no shape above sorts TEXT at all: they order by `k INT`, `n
# NUMERIC`, `b BYTEA`. The text ordering path -- the one v7.38.19 changed
# twice -- had no cell.
#
# These are held to a CEILING rather than to parity, because SPG loses
# them today and a gate that cannot be met is a gate that gets skipped.
# The ceiling is a tripwire against getting worse, and it is written down
# so that lowering it is a decision somebody makes on purpose.
# 3.0, from a measurement rather than a preference: the worst cell is the
# text sort at 2.43x, taken on a box whose load average was 11.3, and a
# ceiling a hair above the worst reading on a loaded machine is a ceiling
# that flaps. A flapping gate is worse than none -- it teaches the reader
# to skip the line, which is how `gates` came to be over budget on every
# run for months without anyone acting.
SORT_CEILING="${SORT_CEILING:-3.0}"
SORT_SHAPES=(
  'sort only, int|SELECT count(*) FROM (SELECT k FROM @T@ ORDER BY k) z'
  'sort only, two keys|SELECT count(*) FROM (SELECT k FROM @T@ ORDER BY k, id) z'
  # `pad`: twenty-six values of two hundred identical characters. Kept
  # because it is what the existing ORDER BY cells project, so its cost
  # is part of theirs -- but it is the degenerate case and is labelled.
  'sort only, text (26 values)|SELECT count(*) FROM (SELECT pad FROM @T@ ORDER BY pad) z'
  # The varied fixture, both key regimes.
  'sort only, short text distinct|SELECT count(*) FROM (SELECT s_short FROM @S@ ORDER BY s_short) z'
  'sort only, long text distinct|SELECT count(*) FROM (SELECT s_long FROM @S@ ORDER BY s_long) z'
  'sort only, long text top-N|SELECT count(*) FROM (SELECT s_long FROM @S@ ORDER BY s_long LIMIT 10) z'
)

SHAPES=(
  'wide, non-indexed key|SELECT pad FROM @T@ ORDER BY k'
  'narrow, non-indexed key|SELECT id FROM @T@ ORDER BY k'
  'indexed key|SELECT pad FROM @T@ ORDER BY id'
  'top-N LIMIT 10|SELECT pad FROM @T@ ORDER BY k LIMIT 10'
  'two keys|SELECT pad FROM @T@ ORDER BY k, id'
  'descending|SELECT pad FROM @T@ ORDER BY k DESC'
  'distinct then order|SELECT DISTINCT k FROM @T@ ORDER BY k'
  'filtered then order|SELECT pad FROM @T@ WHERE id % 3 = 0 ORDER BY k'
)

# r1042 — the NUMERIC / BYTEA panel. v7.37.26 gave both types an index
# key and rebuilt the NUMERIC sort key, and none of that surface was
# visible here: the fixture above has neither column, so every one of
# these shapes was shipped unmeasured against PG18.
#
# `@N@` is the typed fixture.
TYPED_SHAPES=(
  'numeric key|SELECT id FROM @N@ ORDER BY n'
  'numeric wide|SELECT pad FROM @N@ ORDER BY n'
  'numeric distinct|SELECT DISTINCT n FROM @N@ ORDER BY n'
  'numeric top-N|SELECT pad FROM @N@ ORDER BY n LIMIT 10'
  'numeric equality|SELECT count(*) FROM @N@ WHERE n = 1.23'
  'numeric range|SELECT count(*) FROM @N@ WHERE n BETWEEN 1 AND 2'
  'bytea key|SELECT id FROM @N@ ORDER BY b'
  # Dollar-quoted so the shape reads the way a person would write it.
  # It was spelled with chr() to dodge this file's own quoting, and that
  # spelling folded differently from the ordinary one — the panel is for
  # measuring what users send, not what the harness found easy to quote.
  'bytea equality|SELECT count(*) FROM @N@ WHERE b = decode(lpad(to_hex(7), 16, $q$0$q$), $q$hex$q$)'
)

LOSSES=0; CELLS=0; CONTROL_DIFFS=0; DEMOTED=0

printf '\n%-8s %-26s %-16s %-16s %s\n' SIZE SHAPE 'SPGS(min-max)' 'PG18(min-max)' VERDICT
printf '%-8s %-26s %-16s %-16s %s\n' -------- -------------------------- ---------------- ---------------- -------

for rows in ${SIZES}; do
  T="sweep_${rows}"
  NT="sweept_${rows}"
  setup_table "${SPG_URI}" "${T}" "${rows}"
  setup_table "${PG_URI}"  "${T}" "${rows}"
  setup_typed_table "${SPG_URI}" "${NT}" "${rows}"
  setup_typed_table "${PG_URI}"  "${NT}" "${rows}"
  verify_typed_predicates "${SPG_URI}" "${NT}"
  verify_typed_predicates "${PG_URI}"  "${NT}"

  for entry in "${SHAPES[@]}" "${TYPED_SHAPES[@]}"; do
    name="${entry%%|*}"; sql="${entry#*|}"; sql="${sql//@T@/${T}}"; sql="${sql//@N@/${NT}}"
    # Three legs, not two: SPG, PG, and SPG a second time. The third is
    # this cell's own control, and it is timed HERE, between the other
    # two, rather than in a block after every size has finished.
    #
    # v7.38.18 — that block is why this changed. The header has always
    # said the control's differing-cell count IS the run's resolution and
    # that cells inside it report `unresolved`. Neither was true: the
    # count was printed and never consulted, and it was measured at one
    # size, minutes after the cells it was supposed to qualify, on a
    # machine whose load had moved on. A prerelease run called `two keys`
    # at 1,000 rows a LOSS on a 27 microsecond separation while its
    # control -- taken later, in a calmer window -- reported a clean
    # floor. Re-measured on a quiet machine at N=25, the same cell was
    # 0.524-0.645 against PG's 0.516-0.718: no gap at all.
    #
    # A separation the same binary produces against ITSELF, in the same
    # window, on the same shape, is not a verdict about SPG.
    s=(); g=(); c=()
    for ((i = 0; i < N; i++)); do
      # Rule 4: alternate, and rotate which leg starts each round, so no
      # leg is systematically last while the machine drifts.
      case $(( i % 3 )) in
        0)
          s+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
          g+=("$(time_one "${PG_URI}"  "${sql}" "${PG_WM}")")
          c+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
          ;;
        1)
          g+=("$(time_one "${PG_URI}"  "${sql}" "${PG_WM}")")
          c+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
          s+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
          ;;
        *)
          c+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
          s+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
          g+=("$(time_one "${PG_URI}"  "${sql}" "${PG_WM}")")
          ;;
      esac
    done
    smin="$(lo "${s[@]}")"; smax="$(hi "${s[@]}")"
    gmin="$(lo "${g[@]}")"; gmax="$(hi "${g[@]}")"
    cmin="$(lo "${c[@]}")"; cmax="$(hi "${c[@]}")"
    v="$(verdict "${smin}" "${smax}" "${gmin}" "${gmax}")"
    # The same binary against itself. If THAT separates, this cell has no
    # resolution left to spend on a verdict about PG.
    cv="$(verdict "${smin}" "${smax}" "${cmin}" "${cmax}")"
    note=""
    if [[ "${cv}" != unresolved ]]; then
      CONTROL_DIFFS=$((CONTROL_DIFFS + 1))
      if [[ "${v}" != unresolved ]]; then
        note="  <- withdrawn: same binary against itself separated too (${cmin}-${cmax})"
        DEMOTED=$((DEMOTED + 1))
        v="unresolved"
      fi
    fi
    [[ "${v}" == LOSS ]] && LOSSES=$((LOSSES + 1))
    CELLS=$((CELLS + 1))
    printf '%-8s %-26s %-16s %-16s %s%s\n' \
      "${rows}" "${name}" "${smin}-${smax}" "${gmin}-${gmax}" "${v}" "${note}"
  done
done

# The sort panel, at the largest size only: a cost class shows most
# clearly there, and these four cells are extra work on top of the
# sixty-four above.
echo
echo "sort panel — the sort alone, ratio against PG18, ceiling ${SORT_CEILING}x:"
SORT_WORST=0
SORT_OVER=0
BIG="sweep_$(set -- ${SIZES}; for x in "$@"; do :; done; echo "$x")"
BIGN="$(set -- ${SIZES}; for x in "$@"; do :; done; echo "$x")"
SORTT="sortfix_${BIGN}"
setup_sort_table "${SPG_URI}" "${SORTT}" "${BIGN}"
setup_sort_table "${PG_URI}"  "${SORTT}" "${BIGN}"
for entry in "${SORT_SHAPES[@]}"; do
  name="${entry%%|*}"; sql="${entry#*|}"; sql="${sql//@T@/${BIG}}"; sql="${sql//@S@/${SORTT}}"
  s=(); g=()
  for ((i = 0; i < N; i++)); do
    if (( i % 2 == 0 )); then
      s+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
      g+=("$(time_one "${PG_URI}"  "${sql}" "${PG_WM}")")
    else
      g+=("$(time_one "${PG_URI}"  "${sql}" "${PG_WM}")")
      s+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
    fi
  done
  smin="$(lo "${s[@]}")"; gmin="$(lo "${g[@]}")"
  ratio="$(awk -v a="${smin}" -v b="${gmin}" 'BEGIN{ if (b <= 0) print "0"; else printf "%.2f", a/b }')"
  over="$(awk -v r="${ratio}" -v c="${SORT_CEILING}" 'BEGIN{ print (r > c) ? 1 : 0 }')"
  (( over )) && SORT_OVER=$((SORT_OVER + 1))
  SORT_WORST="$(awk -v a="${ratio}" -v b="${SORT_WORST}" 'BEGIN{ print (a > b) ? a : b }')"
  printf '  %-26s %8s %8s  %sx%s\n' "${name}" "${smin}" "${gmin}" "${ratio}" \
    "$( (( over )) && echo '  <- OVER CEILING' )"
done

echo
echo "load after: $(uptime)"
echo "cells=${CELLS} losses=${LOSSES} control_false_differences=${CONTROL_DIFFS} withdrawn=${DEMOTED} sort_worst=${SORT_WORST}x sort_over_ceiling=${SORT_OVER}"
if (( CONTROL_DIFFS > 0 )); then
  echo "NOTE: on ${CONTROL_DIFFS} cell(s) the binary separated from ITSELF in the same"
  echo "      window. ${DEMOTED} verdict(s) were withdrawn on that ground and report"
  echo "      \`unresolved\`. A machine this busy cannot certify a small difference;"
  echo "      re-run on a quiet box or raise N before acting on any single cell."
fi
(( LOSSES == 0 )) || exit 1
if (( SORT_OVER > 0 )); then
  echo "SORT PANEL: ${SORT_OVER} cell(s) past the ${SORT_CEILING}x ceiling (worst ${SORT_WORST}x)."
  echo "            The sort is known to be behind; getting FURTHER behind is not"
  echo "            allowed to pass quietly. See docs/PERF-FINDING-2026-08-24-sort-and-collation.md"
  exit 1
fi
